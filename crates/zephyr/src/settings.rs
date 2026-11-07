//! The settings registry (settings spec §4.1-4.3): every setting described once, with its type,
//! what makes a change stricter, who may change it and how it is applied. Pure: no I/O and no
//! router types, so the router, the agent's `gateway` tool, the attach methods, the TUI panel and
//! the host CLI all read the same table.

use std::time::Duration;

use toml_edit::{Array, Value};

pub struct Setting {
    /// Dotted `zephyr.toml` key. `mcp.<name>.enabled` stands for every connector (see `find`).
    pub key: &'static str,
    /// Panel section, e.g. "Background tasks".
    pub section: &'static str,
    pub label: &'static str,
    /// One line, shown in the panel and in `/settings KEY`.
    pub help: &'static str,
    pub kind: Kind,
    pub agent: Access,
    /// Loosening needs a strong channel.
    pub strong: bool,
    pub stricter: Stricter,
    pub apply: Apply,
}

pub enum Kind {
    Bool,
    Int {
        min: i64,
        max: i64,
    },
    Float {
        min: f64,
        max: f64,
    },
    Duration {
        min_secs: u64,
        max_secs: u64,
    },
    /// Ordered strictest first.
    Choice(&'static [&'static str]),
    /// `PROVIDER/MODEL`; whether the account lists it is the caller's check.
    Model,
    Text {
        max: usize,
    },
    /// Comma separated names, each one of the listed.
    List(&'static [&'static str]),
    /// `none` or `default` clears the key.
    Optional(&'static Kind),
}

/// Read is implied for every entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Access {
    Never,
    Propose,
    /// Tighten at once, propose a loosening.
    Tighten,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stricter {
    Lower,
    Higher,
    /// A `Choice`: earlier entries are stricter.
    Earlier,
    FalseIsStricter,
    /// A number where lower is stricter and an absent limit is the loosest.
    NoneIsLoosest,
    /// A list: removing entries is stricter, adding is looser.
    Fewer,
    NotOrdered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Apply {
    Toml,
    Model,
    Thinking,
    Subagent,
    Rule,
    /// Private facts (approvals spec A1).
    Facts,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Actor<'a> {
    Owner {
        channel: &'a str,
    },
    Agent {
        session: &'a str,
    },
    /// The revert that ends a time limit: strong.
    Internal,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Apply,
    Propose,
    Refuse(String),
}

#[allow(clippy::too_many_arguments)]
const fn row(
    key: &'static str,
    section: &'static str,
    label: &'static str,
    help: &'static str,
    kind: Kind,
    agent: Access,
    strong: bool,
    stricter: Stricter,
    apply: Apply,
) -> Setting {
    Setting {
        key,
        section,
        label,
        help,
        kind,
        agent,
        strong,
        stricter,
        apply,
    }
}

const READ_ONLY: &str = "Read-only";

const fn fixed(key: &'static str, label: &'static str, help: &'static str) -> Setting {
    row(
        key,
        READ_ONLY,
        label,
        help,
        Kind::Text { max: 0 },
        Access::Never,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    )
}

/// The channels `settings.strong_channels` may name.
pub const CHANNELS: &[&str] = &["tui", "chat", "discord", "imessage"];

/// The thinking levels rpi 0.18 offers, off first. The live list comes from
/// `get_available_thinking_levels`; this is what the registry validates against.
pub const THINKING: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh"];

pub static SETTINGS: &[Setting] = &[
    row(
        "model",
        "Model",
        "Model",
        "The model the active account runs, PROVIDER/MODEL.",
        Kind::Model,
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Model,
    ),
    row(
        "thinking",
        "Model",
        "Thinking level",
        "How much the model thinks before it answers (this session only).",
        Kind::Choice(THINKING),
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Thinking,
    ),
    row(
        "tasks.parallel",
        "Background tasks",
        "Tasks at once",
        "How many background tasks run at the same time.",
        Kind::Int { min: 1, max: 32 },
        Access::Tighten,
        false,
        Stricter::Lower,
        Apply::Toml,
    ),
    row(
        "tasks.model",
        "Background tasks",
        "Task model",
        "The model background tasks use unless they name one; none follows the account.",
        Kind::Optional(&Kind::Model),
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "subagent",
        "Background tasks",
        "Subagents",
        "Named helpers the agent starts tasks as: add, prompt, model, rm.",
        Kind::Text { max: 4000 },
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Subagent,
    ),
    row(
        "budget.daily_tokens",
        "Budget",
        "Daily tokens",
        "Tokens per UTC day before the agent pauses; none is no limit.",
        Kind::Optional(&Kind::Int {
            min: 1,
            max: i64::MAX,
        }),
        Access::Tighten,
        true,
        Stricter::NoneIsLoosest,
        Apply::Toml,
    ),
    row(
        "budget.daily_cost_usd",
        "Budget",
        "Daily cost (USD)",
        "Dollars per UTC day before the agent pauses; none is no limit.",
        Kind::Optional(&Kind::Float {
            min: 0.0,
            max: 1.0e9,
        }),
        Access::Tighten,
        true,
        Stricter::NoneIsLoosest,
        Apply::Toml,
    ),
    row(
        "budget.run_turns",
        "Budget",
        "Turns per run",
        "Model turns one run may take; none is no limit.",
        Kind::Optional(&Kind::Int {
            min: 1,
            max: 100_000,
        }),
        Access::Tighten,
        true,
        Stricter::NoneIsLoosest,
        Apply::Toml,
    ),
    row(
        "budget.run_minutes",
        "Budget",
        "Minutes per run",
        "Minutes one main-session run may take; none is no limit.",
        Kind::Optional(&Kind::Float {
            min: 0.1,
            max: 100_000.0,
        }),
        Access::Tighten,
        true,
        Stricter::NoneIsLoosest,
        Apply::Toml,
    ),
    row(
        "timezone",
        "General",
        "Time zone",
        "IANA name for job schedules, e.g. Asia/Seoul; none follows the system.",
        Kind::Optional(&Kind::Text { max: 64 }),
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "auth.enabled",
        "Accounts",
        "Token refresh",
        "Refresh expiring sign-ins in the background.",
        Kind::Bool,
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "auth.refresh_minutes",
        "Accounts",
        "Refresh check (minutes)",
        "How often to look for expiring sign-ins.",
        Kind::Int { min: 1, max: 60 },
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "mcp.<name>.enabled",
        "Connectors",
        "Connector enabled",
        "Whether the agent may call this connector.",
        Kind::Bool,
        Access::Tighten,
        false,
        Stricter::FalseIsStricter,
        Apply::Toml,
    ),
    row(
        "rule",
        "Rules",
        "Standing rules",
        "Rules the agent follows in every run: add, rm.",
        Kind::Text { max: 4000 },
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Rule,
    ),
    row(
        "settings.strong_channels",
        "Security",
        "Strong channels",
        "Channels that may loosen a protected setting; the agent may only remove entries.",
        Kind::List(CHANNELS),
        Access::Tighten,
        true,
        Stricter::Fewer,
        Apply::Toml,
    ),
    fixed(
        "owners",
        "Owners",
        "Who may talk to the agent, per channel.",
    ),
    fixed("discord", "Discord", "The Discord adapter."),
    fixed("imessage", "iMessage", "The BlueBubbles adapter."),
    fixed("attach", "Attach", "The rpi --connect listener."),
    fixed(
        "agent.env",
        "Agent environment",
        "Variables the agent's processes get.",
    ),
    fixed("extensions", "Extensions", "Extra rpi extensions."),
    fixed(
        "policy_plugin",
        "Policy plugin",
        "The zephyr-policy library.",
    ),
    fixed("rpi", "rpi", "The rpi binary."),
    fixed("home", "Home", "The agent's home folder."),
    fixed("state", "State", "The daemon's state folder."),
    fixed(
        "account",
        "Accounts",
        "The [[account]] pool, in failover order.",
    ),
];

/// The setting for a dotted key. `mcp.NAME.enabled` finds the connector entry.
pub fn find(key: &str) -> Option<&'static Setting> {
    if let Some(name) = key
        .strip_prefix("mcp.")
        .and_then(|k| k.strip_suffix(".enabled"))
    {
        return (!name.is_empty() && !name.contains('.'))
            .then(|| SETTINGS.iter().find(|s| s.key == "mcp.<name>.enabled"))
            .flatten();
    }
    SETTINGS.iter().find(|s| s.key == key)
}

fn is_clear(raw: &str) -> bool {
    matches!(raw.trim(), "none" | "default")
}

/// Whether `raw` clears an `Optional` setting (`none` / `default`). `parse` does not take these.
pub fn clears(s: &Setting, raw: &str) -> bool {
    matches!(s.kind, Kind::Optional(_)) && is_clear(raw)
}

/// The value `raw` stands for, checked against the setting's kind. A clearing word on an
/// `Optional` is not a value: test `clears` first.
pub fn parse(s: &Setting, raw: &str) -> Result<Value, String> {
    if s.section == READ_ONLY {
        return Err(format!("{} is read-only", s.key));
    }
    let value = parse_kind(&s.kind, raw.trim(), s.key)?;
    if s.key == "settings.strong_channels" && !list_names(raw).contains(&"tui") {
        return Err("tui stays a strong channel".into());
    }
    Ok(value)
}

fn parse_kind(kind: &Kind, raw: &str, key: &str) -> Result<Value, String> {
    match kind {
        Kind::Bool => match raw.to_ascii_lowercase().as_str() {
            "true" | "on" | "yes" => Ok(Value::from(true)),
            "false" | "off" | "no" => Ok(Value::from(false)),
            _ => Err(format!("{key} is on or off, not {raw:?}")),
        },
        Kind::Int { min, max } => {
            let n: i64 = raw
                .parse()
                .map_err(|_| format!("{key} is a whole number, not {raw:?}"))?;
            if n < *min || n > *max {
                return Err(format!("{key} must be {min} to {max}"));
            }
            Ok(Value::from(n))
        }
        Kind::Float { min, max } => {
            let n: f64 = raw
                .parse()
                .map_err(|_| format!("{key} is a number, not {raw:?}"))?;
            if !n.is_finite() || n < *min || n > *max {
                return Err(format!("{key} must be {min} to {max}"));
            }
            Ok(Value::from(n))
        }
        Kind::Duration { min_secs, max_secs } => {
            let d = duration(raw)?;
            if d.as_secs() < *min_secs || d.as_secs() > *max_secs {
                return Err(format!("{key} must be {min_secs}s to {max_secs}s"));
            }
            Ok(Value::from(d.as_secs() as i64))
        }
        Kind::Choice(options) => options
            .iter()
            .find(|o| o.eq_ignore_ascii_case(raw))
            .map(|o| Value::from(*o))
            .ok_or_else(|| format!("{key} is one of {}", options.join(", "))),
        Kind::Model => {
            if zephyr_policy::tools::model_shape(raw) {
                Ok(Value::from(raw))
            } else {
                Err(format!("{key} is PROVIDER/MODEL, not {raw:?}"))
            }
        }
        Kind::Text { max } => {
            if raw.is_empty() || raw.chars().count() > *max {
                return Err(format!("{key} must be 1 to {max} characters"));
            }
            Ok(Value::from(raw))
        }
        Kind::List(allowed) => {
            let names = list_names(raw);
            if let Some(bad) = names.iter().find(|n| !allowed.contains(n)) {
                return Err(format!(
                    "{key} names {bad:?}; allowed: {}",
                    allowed.join(", ")
                ));
            }
            let mut arr = Array::new();
            for n in names {
                arr.push(n);
            }
            Ok(Value::from(arr))
        }
        Kind::Optional(inner) => {
            if is_clear(raw) {
                return Err(format!("{key} is cleared, not set, by {raw:?}"));
            }
            parse_kind(inner, raw, key)
        }
    }
}

fn list_names(raw: &str) -> Vec<&str> {
    let mut out: Vec<&str> = Vec::new();
    for n in raw.split(',').map(str::trim).filter(|n| !n.is_empty()) {
        if !out.contains(&n) {
            out.push(n);
        }
    }
    out
}

/// `30s`, `5m`, `2h`, `1d`.
fn duration(raw: &str) -> Result<Duration, String> {
    let raw = raw.trim();
    let bad = || format!("{raw:?} is not a duration like 30s, 5m, 2h or 1d");
    let (num, unit) = [("s", 1), ("m", 60), ("h", 3600), ("d", 86_400)]
        .into_iter()
        .find_map(|(u, n)| raw.strip_suffix(u).map(|num| (num, n)))
        .ok_or_else(bad)?;
    if num.is_empty() || !num.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let n: u64 = num.parse().map_err(|_| bad())?;
    n.checked_mul(unit)
        .map(Duration::from_secs)
        .ok_or_else(|| format!("{raw:?} is too long"))
}

const MAX_UNTIL: Duration = Duration::from_secs(30 * 86_400);

/// A time limit (`for 2h`): `min` (30 s unless a test lowers it) to 30 days.
pub fn parse_until(raw: &str, min: Duration) -> Result<Duration, String> {
    let d = duration(raw)?;
    if d < min || d > MAX_UNTIL {
        return Err(format!(
            "a time limit is {}s to 30d, not {}",
            min.as_secs(),
            raw.trim()
        ));
    }
    Ok(d)
}

/// What a value is, for comparing two of them.
enum Rank {
    Num(f64),
    Flag(bool),
    Index(usize),
    Set(Vec<String>),
}

fn rank(kind: &Kind, raw: &str) -> Option<Rank> {
    match kind {
        Kind::Bool => parse_kind(kind, raw, "").ok()?.as_bool().map(Rank::Flag),
        Kind::Int { .. } => Some(Rank::Num(
            parse_kind(kind, raw, "").ok()?.as_integer()? as f64
        )),
        Kind::Float { .. } => Some(Rank::Num(parse_kind(kind, raw, "").ok()?.as_float()?)),
        Kind::Duration { .. } => Some(Rank::Num(
            parse_kind(kind, raw, "").ok()?.as_integer()? as f64
        )),
        Kind::Choice(options) => options
            .iter()
            .position(|o| o.eq_ignore_ascii_case(raw.trim()))
            .map(Rank::Index),
        Kind::List(_) => {
            parse_kind(kind, raw, "").ok()?;
            Some(Rank::Set(
                list_names(raw).into_iter().map(String::from).collect(),
            ))
        }
        Kind::Optional(inner) => rank(inner, raw),
        Kind::Model | Kind::Text { .. } => None,
    }
}

/// Whether going from `old` to `new` is no looser than before (`None` is an absent value), or
/// `None` when the setting has no order or a value does not parse. Equal values are not looser.
pub fn tighter(s: &Setting, old: Option<&str>, new: Option<&str>) -> Option<bool> {
    let old = old.map(str::trim).filter(|o| !is_clear(o));
    let new = new.map(str::trim).filter(|n| !is_clear(n));
    if s.stricter == Stricter::NotOrdered {
        return None;
    }
    if s.stricter == Stricter::NoneIsLoosest {
        match (old, new) {
            (None, None) => return Some(true),
            (_, None) => return Some(false),
            (None, Some(_)) => return Some(true),
            _ => {}
        }
    }
    let (old, new) = (old?, new?);
    let (a, b) = (rank(&s.kind, old)?, rank(&s.kind, new)?);
    match (s.stricter, a, b) {
        (Stricter::Lower | Stricter::NoneIsLoosest, Rank::Num(a), Rank::Num(b)) => Some(b <= a),
        (Stricter::Higher, Rank::Num(a), Rank::Num(b)) => Some(b >= a),
        (Stricter::Earlier, Rank::Index(a), Rank::Index(b)) => Some(b <= a),
        (Stricter::FalseIsStricter, Rank::Flag(a), Rank::Flag(b)) => Some(!b || a),
        (Stricter::Fewer, Rank::Set(a), Rank::Set(b)) => Some(b.iter().all(|n| a.contains(n))),
        _ => None,
    }
}

/// Whether the change is a loosening, the only way callers compute `decide`'s `loosens`
/// argument: a setting with no order, or a value that does not compare, counts as loosening.
pub fn loosens(s: &Setting, old: Option<&str>, new: Option<&str>) -> bool {
    tighter(s, old, new) != Some(true)
}

/// What the actor's change to `s` comes to. `loosens` is the caller's answer from `tighter`
/// (an unordered setting that cannot be compared counts as loosening).
pub fn decide(s: &Setting, actor: &Actor, loosens: bool, strong_channels: &[String]) -> Verdict {
    if s.section == READ_ONLY {
        return Verdict::Refuse(format!("{} is read-only; edit zephyr.toml", s.key));
    }
    match actor {
        Actor::Internal => Verdict::Apply,
        Actor::Owner { channel } => {
            if strong_channels.iter().any(|c| c == channel) || !(s.strong && loosens) {
                Verdict::Apply
            } else {
                Verdict::Refuse(format!(
                    "loosening {} needs {}",
                    s.key,
                    strong_channels.join(" or ")
                ))
            }
        }
        Actor::Agent { .. } => match s.agent {
            Access::Never => Verdict::Refuse(format!("{} is not the agent's to change", s.key)),
            Access::Tighten if !loosens => Verdict::Apply,
            Access::Tighten | Access::Propose => Verdict::Propose,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strong() -> Vec<String> {
        vec!["tui".into(), "chat".into()]
    }

    fn sample(kind: &Kind) -> (String, Vec<String>) {
        match kind {
            Kind::Bool => ("true".into(), vec!["maybe".into()]),
            Kind::Int { min, max } => (
                min.to_string(),
                min.checked_sub(1)
                    .into_iter()
                    .chain(max.checked_add(1))
                    .map(|n| n.to_string())
                    .chain(["x".to_string()])
                    .collect(),
            ),
            Kind::Float { min, max } => (
                min.to_string(),
                vec![
                    (min - 1.0).to_string(),
                    (max + 1.0).to_string(),
                    "NaN".into(),
                ],
            ),
            Kind::Duration { min_secs, max_secs } => (
                format!("{min_secs}s"),
                vec![format!("{}s", min_secs - 1), format!("{}s", max_secs + 1)],
            ),
            Kind::Choice(o) => (o[0].into(), vec!["nonsense".into()]),
            Kind::Model => ("anthropic/claude-sonnet-5".into(), vec!["sonnet".into()]),
            Kind::Text { max } => ("x".into(), vec![String::new(), "x".repeat(max + 1)]),
            Kind::List(o) => (o[0].into(), vec!["nowhere".into()]),
            Kind::Optional(inner) => sample(inner),
        }
    }

    #[test]
    fn every_registered_key_parses_its_own_default_and_rejects_out_of_bounds() {
        for s in SETTINGS.iter().filter(|s| s.section != READ_ONLY) {
            let (good, bad) = sample(&s.kind);
            assert!(parse(s, &good).is_ok(), "{} rejected {good}", s.key);
            for b in bad {
                assert!(parse(s, &b).is_err(), "{} accepted {b:?}", s.key);
            }
            if matches!(s.kind, Kind::Optional(_)) {
                assert!(clears(s, "none") && clears(s, "default"), "{}", s.key);
            }
        }
        let p = find("tasks.parallel").unwrap();
        assert!(parse(p, "2").is_ok());
        assert!(parse(p, "0").is_err());
        assert!(parse(p, "33").is_err());
        assert!(parse(find("owners").unwrap(), "x").is_err());
    }

    #[test]
    fn keys_are_unique_and_connectors_match_by_prefix() {
        for (i, s) in SETTINGS.iter().enumerate() {
            assert!(SETTINGS[..i].iter().all(|o| o.key != s.key), "{}", s.key);
        }
        assert_eq!(
            find("mcp.notion.enabled").unwrap().key,
            "mcp.<name>.enabled"
        );
        assert!(find("mcp..enabled").is_none());
        assert!(find("mcp.a.b.enabled").is_none());
        assert!(find("nope").is_none());
    }

    #[test]
    fn strictness_follows_each_kind() {
        let cost = find("budget.daily_cost_usd").unwrap();
        assert_eq!(tighter(cost, Some("5"), Some("3")), Some(true));
        assert_eq!(tighter(cost, Some("3"), Some("5")), Some(false));
        assert_eq!(tighter(cost, Some("5"), None), Some(false));
        assert_eq!(tighter(cost, Some("5"), Some("none")), Some(false));
        assert_eq!(tighter(cost, None, Some("5")), Some(true));
        let par = find("tasks.parallel").unwrap();
        assert_eq!(tighter(par, Some("4"), Some("2")), Some(true));
        assert_eq!(tighter(par, Some("2"), Some("4")), Some(false));
        let mcp = find("mcp.notion.enabled").unwrap();
        assert_eq!(tighter(mcp, Some("true"), Some("false")), Some(true));
        assert_eq!(tighter(mcp, Some("false"), Some("true")), Some(false));
        let ch = find("settings.strong_channels").unwrap();
        assert_eq!(tighter(ch, Some("tui,chat"), Some("tui")), Some(true));
        assert_eq!(tighter(ch, Some("tui"), Some("tui,discord")), Some(false));
        assert_eq!(
            tighter(find("timezone").unwrap(), Some("UTC"), Some("Asia/Seoul")),
            None
        );
        assert_eq!(tighter(find("model").unwrap(), None, Some("a/b")), None);
    }

    #[test]
    fn loosens_is_anything_not_known_to_be_no_looser() {
        let tz = find("timezone").unwrap();
        assert!(loosens(tz, Some("UTC"), Some("Asia/Seoul")));
        let par = find("tasks.parallel").unwrap();
        assert!(!loosens(par, Some("2"), Some("2")));
        assert!(!loosens(par, Some("4"), Some("2")));
        assert!(loosens(par, Some("2"), Some("4")));
    }

    #[test]
    fn non_ascii_durations_are_errors_not_panics() {
        let min = Duration::from_secs(30);
        for raw in ["5분", "30초", "３０s", "é", "s", "-5s", "+5s"] {
            assert!(parse_until(raw, min).is_err(), "{raw}");
        }
    }

    #[test]
    fn strong_channels_always_keep_the_tui() {
        let s = find("settings.strong_channels").unwrap();
        assert!(parse(s, "tui").is_ok());
        assert!(parse(s, "chat,tui").is_ok());
        assert_eq!(parse(s, "chat").unwrap_err(), "tui stays a strong channel");
        assert_eq!(parse(s, "").unwrap_err(), "tui stays a strong channel");
    }

    #[test]
    fn choice_order_is_strictest_first() {
        static MODES: Setting = row(
            "t.mode",
            "T",
            "T",
            "",
            Kind::Choice(&["ask", "hold", "allow"]),
            Access::Tighten,
            false,
            Stricter::Earlier,
            Apply::Toml,
        );
        assert_eq!(tighter(&MODES, Some("allow"), Some("ask")), Some(true));
        assert_eq!(tighter(&MODES, Some("ask"), Some("allow")), Some(false));
        assert_eq!(tighter(&MODES, Some("ask"), Some("bogus")), None);
    }

    #[test]
    fn actors_decide_as_the_spec_says() {
        let cost = find("budget.daily_cost_usd").unwrap();
        let tui = Actor::Owner { channel: "tui" };
        let discord = Actor::Owner { channel: "discord" };
        assert_eq!(decide(cost, &tui, true, &strong()), Verdict::Apply);
        assert_eq!(
            decide(cost, &discord, true, &strong()),
            Verdict::Refuse("loosening budget.daily_cost_usd needs tui or chat".into())
        );
        assert_eq!(decide(cost, &discord, false, &strong()), Verdict::Apply);
        let tz = find("timezone").unwrap();
        assert_eq!(decide(tz, &discord, true, &strong()), Verdict::Apply);
        let agent = Actor::Agent { session: "main" };
        assert_eq!(decide(cost, &agent, false, &strong()), Verdict::Apply);
        assert_eq!(decide(cost, &agent, true, &strong()), Verdict::Propose);
        assert_eq!(decide(tz, &agent, false, &strong()), Verdict::Propose);
        let owners = find("owners").unwrap();
        assert!(matches!(
            decide(owners, &agent, false, &strong()),
            Verdict::Refuse(_)
        ));
        assert!(matches!(
            decide(owners, &tui, false, &strong()),
            Verdict::Refuse(_)
        ));
        static NEVER: Setting = row(
            "t.never",
            "T",
            "T",
            "",
            Kind::Bool,
            Access::Never,
            false,
            Stricter::NotOrdered,
            Apply::Toml,
        );
        assert!(matches!(
            decide(&NEVER, &agent, false, &strong()),
            Verdict::Refuse(_)
        ));
        assert_eq!(
            decide(cost, &Actor::Internal, true, &strong()),
            Verdict::Apply
        );
    }

    #[test]
    fn durations_parse_within_30s_and_30d() {
        let min = Duration::from_secs(30);
        assert_eq!(parse_until("2h", min), Ok(Duration::from_secs(7200)));
        assert!(parse_until("30s", min).is_ok());
        assert!(parse_until("30d", min).is_ok());
        assert!(parse_until("29s", min).is_err());
        assert!(parse_until("31d", min).is_err());
        assert!(parse_until("soon", min).is_err());
        assert!(parse_until("", min).is_err());
        assert!(parse_until("5s", Duration::from_secs(1)).is_ok());
    }
}
