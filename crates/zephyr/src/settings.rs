//! The settings registry (settings spec §4.1-4.3): every setting described once, with its type,
//! what makes a change stricter, who may change it and how it is applied. Pure: no I/O and no
//! router types, so the router, the agent's `gateway` tool, the attach methods, the TUI panel and
//! the host CLI all read the same table. The router's `set_setting` is the one place a change
//! is decided, applied and logged; the texts it shows are built here.

use std::time::Duration;

use toml_edit::{Array, DocumentMut, Item, Value};

use crate::config::Config;

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

/// What `set_setting` did. `old` and `new` are the values in effect before and after, as
/// replies show them (`None`: unset); `applied` is how the change took effect (the reload's
/// answer, or the command's own reply for a subagent or a rule). `Refused` holds the reason,
/// which the owner reads after `[zephyr] `.
#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Changed {
        old: Option<String>,
        new: Option<String>,
        applied: String,
    },
    Proposed {
        id: i64,
    },
    Refused(String),
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

/// rpi 0.18's thinking levels (`rpi_ai::ThinkingLevel`), off first. A model offers some of
/// them: the router checks a level against the child's `get_available_thinking_levels`, which
/// is authoritative; this list only describes the setting.
pub const THINKING: &[&str] = &["off", "minimal", "low", "medium", "high", "xhigh", "max"];

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
    let value = parse_kind(&s.kind, raw.trim(), &named(s.key))?;
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
                Err(format!("{raw} is not PROVIDER/MODEL"))
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

/// A key as `Config::validate` names it in its errors: `tasks.parallel` is `[tasks] parallel`.
fn named(key: &str) -> String {
    match key.split_once('.') {
        Some((table, rest)) => format!("[{table}] {rest}"),
        None => key.to_string(),
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

/// The connector a `mcp.NAME.enabled` key names.
fn connector(key: &str) -> Option<&str> {
    let name = key.strip_prefix("mcp.")?.strip_suffix(".enabled")?;
    (!name.is_empty() && !name.contains('.')).then_some(name)
}

/// The value of `key` in effect in `cfg`, as replies show it: a default when the file has
/// none, `None` when it is unset (a limit absent is no limit). The model is the first
/// account's; the router knows the one the child runs on, and rpi's thinking level.
pub fn value(cfg: &Config, key: &str) -> Option<String> {
    let budget = cfg.budget.as_ref();
    match key {
        "model" => (cfg.accounts.first().map(|a| a.model.clone())).or_else(|| cfg.model.clone()),
        "tasks.parallel" => Some(cfg.parallel_tasks().to_string()),
        "tasks.model" => cfg.task_model().map(String::from),
        "budget.daily_tokens" => budget?.daily_tokens.map(|v| v.to_string()),
        "budget.daily_cost_usd" => budget?.daily_cost_usd.map(|v| v.to_string()),
        "budget.run_turns" => budget?.run_turns.map(|v| v.to_string()),
        "budget.run_minutes" => budget?.run_minutes.map(|v| v.to_string()),
        "timezone" => cfg.timezone.clone(),
        "auth.enabled" => Some(cfg.auth.enabled.to_string()),
        "auth.refresh_minutes" => Some(cfg.auth.refresh_minutes.to_string()),
        "settings.strong_channels" => Some(cfg.settings.strong_channels.join(", ")),
        key => {
            let m = cfg
                .mcp
                .iter()
                .find(|m| Some(m.name.as_str()) == connector(key))?;
            Some(m.enabled.unwrap_or(true).to_string())
        }
    }
}

/// How a parsed value reads in a reply, as `value` writes the same setting.
pub fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.value().clone(),
        Value::Integer(n) => n.value().to_string(),
        Value::Float(f) => f.value().to_string(),
        Value::Boolean(b) => b.value().to_string(),
        Value::Array(a) => (a.iter().filter_map(Value::as_str))
            .collect::<Vec<_>>()
            .join(", "),
        other => other.to_string().trim().to_string(),
    }
}

/// `key` as `zephyr.toml` holds it (`None`: absent), for a proposal to be checked against at
/// `/ok`. `mcp.NAME.enabled` is that `[[mcp]]` table's.
pub fn read(doc: &DocumentMut, key: &str) -> Option<String> {
    let Some(name) = connector(key) else {
        return crate::host::conf::get(doc, key);
    };
    let list = doc.get("mcp")?.as_array_of_tables()?;
    let t = list
        .iter()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(name))?;
    t.get("enabled")?.as_bool().map(|b| b.to_string())
}

/// Sets `key` to `value` in `doc`, or removes it (`None`, back to its default). A value already
/// there keeps its comment.
pub fn write(doc: &mut DocumentMut, key: &str, value: Option<&Value>) -> anyhow::Result<()> {
    if let Some(name) = connector(key) {
        let list = doc.get_mut("mcp").and_then(Item::as_array_of_tables_mut);
        let t = list
            .and_then(|l| {
                l.iter_mut()
                    .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
            })
            .ok_or_else(|| anyhow::anyhow!("no connector named {name}"))?;
        match value {
            Some(v) => t["enabled"] = Item::Value(v.clone()),
            None => drop(t.remove("enabled")),
        }
        return Ok(());
    }
    if let Some(v) = value {
        return crate::host::conf::set(doc, key, &v.to_string());
    }
    let (path, last) = key.rsplit_once('.').unwrap_or(("", key));
    let mut item = doc.as_item_mut();
    for part in path.split('.').filter(|p| !p.is_empty()) {
        match item.get_mut(part) {
            Some(next) => item = next,
            None => return Ok(()),
        }
    }
    if let Some(t) = item.as_table_like_mut() {
        t.remove(last);
    }
    Ok(())
}

/// `/set`'s argument, `KEY VALUE [for D]`, as its three parts. `None` without a value.
pub fn set_args(arg: &str) -> Option<(&str, &str, Option<&str>)> {
    let (key, rest) = arg.trim().split_once(char::is_whitespace)?;
    let rest = rest.trim();
    let (value, until) = match rest.rsplit_once(" for ") {
        Some((v, d)) if !v.trim().is_empty() && !d.trim().contains(char::is_whitespace) => {
            (v.trim(), Some(d.trim()))
        }
        _ => (rest, None),
    };
    (!value.is_empty()).then_some((key, value, until))
}

/// A time limit as the owner typed it: `90s`, `30m`, `2h`, `1d`.
pub fn span(d: Duration) -> String {
    let s = d.as_secs();
    match s {
        0 => "0s".into(),
        s if s % 86_400 == 0 => format!("{}d", s / 86_400),
        s if s % 3600 == 0 => format!("{}h", s / 3600),
        s if s % 60 == 0 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// `at` (unix seconds) as `HH:MM` in `tz`.
pub fn clock(at: i64, tz: &jiff::tz::TimeZone) -> String {
    jiff::Timestamp::from_second(at)
        .map(|t| t.to_zoned(tz.clone()).strftime("%H:%M").to_string())
        .unwrap_or_default()
}

/// How an unset value reads.
pub fn shown(v: Option<&str>) -> &str {
    v.unwrap_or("none")
}

/// The longest agent reason a card or notice quotes (settings spec §4.4).
const REASON_CHARS: usize = 200;
/// The longest proposed command a card shows.
const CARD_CHARS: usize = zephyr_policy::tools::CARD_CHARS;

/// `text` cut at `max` characters.
fn cut(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((i, _)) => format!("{}…", &text[..i]),
        None => text.to_string(),
    }
}

/// What the agent says, on one line and cut: it can never pass for a line of the daemon's own.
fn quoted(text: &str) -> String {
    let clean: String = (text.chars())
        .map(|c| {
            match c.is_control() || matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}') {
                true => ' ',
                false => c,
            }
        })
        .collect();
    cut(
        &clean.split_whitespace().collect::<Vec<_>>().join(" "),
        REASON_CHARS,
    )
}

/// The reply to a change: `[zephyr] KEY OLD → NEW`, with ` until HH:MM` when it is timed.
pub fn changed(key: &str, old: Option<&str>, new: Option<&str>, until: Option<&str>) -> String {
    let until = until.map_or(String::new(), |t| format!(" until {t}"));
    format!("[zephyr] {key} {} → {}{until}", shown(old), shown(new))
}

/// The owner's notice of a tightening the agent applied at once.
pub fn tightened(key: &str, old: Option<&str>, new: Option<&str>, reason: Option<&str>) -> String {
    let says = reason.map_or(String::new(), |r| format!(" (it says: \"{}\")", quoted(r)));
    let old = shown(old);
    format!(
        "[zephyr] the agent set {key} {old} → {}{says}; /set {key} {old} puts it back",
        shown(new)
    )
}

/// The card for an agent's proposal #`id`: `what` is `KEY OLD → NEW`, or the proposed command
/// for a subagent or a rule, as it will run (cut at 1500 characters). `needs` names the strong
/// channels when approving it needs one.
pub fn card(
    id: i64,
    what: &str,
    span: Option<&str>,
    reason: Option<&str>,
    needs: Option<&str>,
) -> String {
    let span = span.map_or(String::new(), |d| format!(" (for {d})"));
    let says = reason.map_or(String::new(), |r| {
        format!("the agent says: \"{}\"\n", quoted(r))
    });
    let needs = needs.map_or(String::new(), |n| format!(" · this one needs {n}"));
    format!(
        "[zephyr] the agent proposes #{id}: {}{span}\n{says}/ok {id} · /no {id}{needs}",
        cut(what, CARD_CHARS)
    )
}

/// The notice that a time limit ended and `key` is back at `old`.
pub fn reverted(key: &str, old: Option<&str>, span: &str) -> String {
    format!(
        "[zephyr] {key} went back to {} (the {span} you set ended)",
        shown(old)
    )
}

/// The answer to `/ok` on a proposal whose setting moved on since the owner was shown it.
pub fn stale(key: &str, current: Option<&str>) -> String {
    format!("[zephyr] {key} is now {}; propose again", shown(current))
}

/// The panel's source column for the latest change (settings spec §4.6).
pub fn source(change: &crate::store::SettingChange) -> String {
    match (change.actor.as_str(), change.proposal) {
        ("agent", Some(n)) => format!("agent (#{n})"),
        ("agent", None) => "agent".into(),
        _ => "you".into(),
    }
}

/// One line of `/settings`: `KEY · VALUE · SOURCE`.
pub struct Row {
    pub key: String,
    pub value: String,
    pub source: String,
}

impl Row {
    fn line(&self) -> String {
        format!("{} · {} · {}", self.key, self.value, self.source)
    }
}

/// What a setting takes, in words.
fn allowed(s: &Setting) -> String {
    fn kind(k: &Kind) -> String {
        match k {
            Kind::Bool => "on or off".into(),
            Kind::Int { min, max: i64::MAX } => format!("a whole number, {min} or more"),
            Kind::Int { min, max } => format!("a whole number, {min} to {max}"),
            Kind::Float { min, max } => format!("a number, {min} to {max}"),
            Kind::Duration { .. } => "a duration like 30s, 5m, 2h or 1d".into(),
            Kind::Choice(o) => format!("one of {}", o.join(", ")),
            Kind::Model => "PROVIDER/MODEL".into(),
            Kind::Text { max } => format!("text, up to {max} characters"),
            Kind::List(o) => format!("a comma separated list of {}", o.join(", ")),
            Kind::Optional(inner) => format!("{}, or none", kind(inner)),
        }
    }
    match s.apply {
        Apply::Thinking => "one of the levels the model offers (/thinking lists them)".into(),
        Apply::Subagent => "/subagent add NAME [PROVIDER/MODEL] PROMPT, model, prompt, rm".into(),
        Apply::Rule => "/rule TEXT, /rule rm N".into(),
        _ => kind(&s.kind),
    }
}

/// Who may change a setting, in words.
fn access(s: &Setting, strong: &[String]) -> String {
    let agent = match s.agent {
        Access::Never => "the agent can read it, not change it",
        Access::Propose => "the agent may propose a change for you to approve",
        Access::Tighten => "the agent may tighten it at once and propose anything else",
    };
    match s.strong {
        true => format!("{agent}; loosening it needs {}", strong.join(" or ")),
        false => agent.into(),
    }
}

/// `/settings` (every section), `/settings SECTION` or `/settings KEY`. `rows` gives a
/// setting's current rows (a connector setting has one per connector); `strong` names the
/// strong channels.
pub fn listing(arg: &str, strong: &[String], rows: impl Fn(&Setting) -> Vec<Row>) -> String {
    let arg = arg.trim();
    let mut sections: Vec<&str> = Vec::new();
    for s in SETTINGS.iter().filter(|s| s.section != READ_ONLY) {
        if !sections.contains(&s.section) {
            sections.push(s.section);
        }
    }
    let section = |name: &str| {
        let lines: Vec<String> = (SETTINGS.iter().filter(|s| s.section == name))
            .flat_map(&rows)
            .map(|r| r.line())
            .collect();
        format!("{name}\n{}", lines.join("\n"))
            .trim_end()
            .to_string()
    };
    if arg.is_empty() {
        let fixed: Vec<&str> = (SETTINGS.iter().filter(|s| s.section == READ_ONLY))
            .map(|s| s.key)
            .collect();
        let all: Vec<String> = sections.iter().map(|n| section(n)).collect();
        return format!(
            "[zephyr] {}\n\nread-only (edit zephyr.toml): {}\n/settings SECTION or KEY says more; \
             /set KEY VALUE [for 2h] changes one",
            all.join("\n\n"),
            fixed.join(", ")
        );
    }
    if let Some(name) = sections.iter().find(|n| n.eq_ignore_ascii_case(arg)) {
        return format!("[zephyr] {}", section(name));
    }
    let Some(s) = find(arg) else {
        return format!("[zephyr] no setting or section {arg}; /settings lists them");
    };
    if s.section == READ_ONLY {
        return format!(
            "[zephyr] {} · read-only\n{}\nedit zephyr.toml to change it",
            s.key, s.help
        );
    }
    let line = match rows(s).into_iter().find(|r| r.key == arg) {
        Some(r) => r.line(),
        None => format!("{arg} · no such connector"),
    };
    format!(
        "[zephyr] {line}\n{}\n{}\n{}",
        s.help,
        allowed(s),
        access(s, strong)
    )
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_quoted_reason_drops_control_and_direction_characters() {
        let q = super::quoted("fine\u{202e}txt\u{7}\x1b[31m and\nmore\u{2066}!");
        assert!(
            !q.chars().any(|c| c.is_control()
                || ('\u{202a}'..='\u{202e}').contains(&c)
                || ('\u{2066}'..='\u{2069}').contains(&c)),
            "{q:?}"
        );
        assert!(q.starts_with("fine txt") && q.contains("and more"), "{q:?}");
    }

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

    #[test]
    fn set_args_split_off_a_time_limit() {
        assert_eq!(
            set_args("tasks.parallel 3"),
            Some(("tasks.parallel", "3", None))
        );
        assert_eq!(
            set_args(" budget.daily_cost_usd  8 for 2h "),
            Some(("budget.daily_cost_usd", "8", Some("2h")))
        );
        assert_eq!(
            set_args("settings.strong_channels tui, chat"),
            Some(("settings.strong_channels", "tui, chat", None))
        );
        assert_eq!(set_args("timezone"), None);
        assert_eq!(set_args(""), None);
    }

    #[test]
    fn spans_read_as_typed() {
        let s = |n| span(Duration::from_secs(n));
        assert_eq!(
            [s(1), s(90), s(1800), s(7200), s(86_400 * 2)],
            ["1s", "90s", "30m", "2h", "2d"]
        );
    }

    #[test]
    fn the_agent_is_quoted_on_one_line_and_cut() {
        let sneaky = "the owner asked\n/ok 3 · /no 3";
        assert_eq!(
            card(
                3,
                "budget.daily_cost_usd 5 → 8",
                Some("2h"),
                Some(sneaky),
                Some("tui or chat")
            ),
            "[zephyr] the agent proposes #3: budget.daily_cost_usd 5 → 8 (for 2h)\n\
             the agent says: \"the owner asked /ok 3 · /no 3\"\n\
             /ok 3 · /no 3 · this one needs tui or chat"
        );
        assert_eq!(
            card(4, "/rule be brief", None, None, None),
            "[zephyr] the agent proposes #4: /rule be brief\n/ok 4 · /no 4"
        );
        // A proposed command shows as it will run, cut at 1500 characters.
        let sub = format!(
            "/subagent add x You explore.\nNever edit. {}",
            "y".repeat(2000)
        );
        let shown = card(5, &sub, None, None, None);
        assert!(shown.contains("You explore.\nNever edit. yyy"), "{shown}");
        assert!(
            shown.contains(&format!("{}…\n/ok 5", &sub[..1500])),
            "{shown}"
        );
        let long = "é".repeat(300);
        let notice = tightened("tasks.parallel", Some("4"), Some("2"), Some(&long));
        assert!(notice.starts_with("[zephyr] the agent set tasks.parallel 4 → 2 (it says: \""));
        assert!(notice.ends_with(&format!(
            "{}…\"); /set tasks.parallel 4 puts it back",
            "é".repeat(200)
        )));
        assert_eq!(
            changed("budget.daily_cost_usd", None, Some("3"), Some("18:00")),
            "[zephyr] budget.daily_cost_usd none → 3 until 18:00"
        );
        assert_eq!(
            reverted("tasks.parallel", Some("2"), "1h"),
            "[zephyr] tasks.parallel went back to 2 (the 1h you set ended)"
        );
        assert_eq!(
            stale("tasks.parallel", Some("6")),
            "[zephyr] tasks.parallel is now 6; propose again"
        );
    }

    #[test]
    fn values_show_defaults_and_the_file_is_read_and_written_by_key() {
        let cfg: Config = toml::from_str(
            "rpi = 'a'\nhome = 'b'\n[budget]\ndaily_cost_usd = 5.0\n\
             [[mcp]]\nname = 'notes'\ncommand = 'n'\n",
        )
        .unwrap();
        assert_eq!(value(&cfg, "tasks.parallel").as_deref(), Some("2"));
        assert_eq!(value(&cfg, "budget.daily_cost_usd").as_deref(), Some("5"));
        assert_eq!(value(&cfg, "budget.daily_tokens"), None);
        assert_eq!(value(&cfg, "auth.refresh_minutes").as_deref(), Some("10"));
        assert_eq!(
            value(&cfg, "settings.strong_channels").as_deref(),
            Some("tui, chat")
        );
        assert_eq!(value(&cfg, "mcp.notes.enabled").as_deref(), Some("true"));
        assert_eq!(value(&cfg, "mcp.other.enabled"), None);
        let cost = find("budget.daily_cost_usd").unwrap();
        assert_eq!(text(&parse(cost, "8").unwrap()), "8");

        let mut doc: DocumentMut =
            "[tasks]\nmodel = 'a/b' # mine\n[[mcp]]\nname = 'notes'\ncommand = 'n'\n"
                .parse()
                .unwrap();
        assert_eq!(read(&doc, "mcp.notes.enabled"), None);
        write(&mut doc, "mcp.notes.enabled", Some(&Value::from(false))).unwrap();
        assert_eq!(read(&doc, "mcp.notes.enabled").as_deref(), Some("false"));
        assert!(write(&mut doc, "mcp.nope.enabled", Some(&Value::from(false))).is_err());
        write(&mut doc, "tasks.model", Some(&Value::from("c/d"))).unwrap();
        assert!(doc.to_string().contains("model = \"c/d\" # mine"), "{doc}");
        write(&mut doc, "tasks.model", None).unwrap();
        write(&mut doc, "budget.daily_tokens", None).unwrap();
        assert_eq!(read(&doc, "tasks.model"), None);
        let list = parse(find("settings.strong_channels").unwrap(), "tui, chat").unwrap();
        write(&mut doc, "settings.strong_channels", Some(&list)).unwrap();
        assert_eq!(
            read(&doc, "settings.strong_channels").as_deref(),
            Some("tui, chat")
        );
        let cfg: Config = toml::from_str(&format!("rpi = 'a'\nhome = 'b'\n{doc}")).unwrap();
        assert_eq!(cfg.mcp[0].enabled, Some(false));
    }

    #[test]
    fn settings_list_a_section_a_key_and_what_is_read_only() {
        let strong = strong();
        let rows = |s: &Setting| {
            vec![Row {
                key: s.key.into(),
                value: "v".into(),
                source: "default".into(),
            }]
        };
        let budget = listing("budget", &strong, rows);
        assert!(budget.starts_with("[zephyr] Budget\nbudget.daily_tokens · v · default\n"));
        let key = listing("budget.daily_cost_usd", &strong, rows);
        assert!(key.contains("a number, 0 to 1000000000, or none"), "{key}");
        assert!(key.ends_with("loosening it needs tui or chat"), "{key}");
        assert!(listing("owners", &strong, rows).contains("read-only"));
        assert!(listing("", &strong, rows).contains("read-only (edit zephyr.toml): owners,"));
        assert!(listing("nope", &strong, rows).starts_with("[zephyr] no setting or section nope"));
    }
}
