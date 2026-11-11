//! The settings registry (settings spec §4.1-4.3): every setting described once, with its type,
//! what makes a change stricter, who may change it and how it is applied. Pure: no I/O and no
//! router types, so the router, the agent's `gateway` tool, the attach methods, the TUI panel and
//! the host CLI all read the same table. The router's `set_setting` is the one place a change
//! is decided, applied and logged; the texts it shows are built here.

use std::time::Duration;

use toml_edit::{Array, DocumentMut, Item, Table, Value};
use zephyr_policy::policy::Class;

use crate::approvals::{FactsMode, Preset};
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
    /// A class's approval mode: `ask`, `ask-new`, `delay N`, `auto`, money's `auto N`
    /// (`approvals::parse_mode`).
    Mode(Class),
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
    /// A list: adding entries is stricter, removing is looser (`approvals.strong_only`).
    More,
    /// An approval mode, ordered by `approvals::stricter`.
    Approval,
    /// A connector tool's class (approvals spec §4.1): only a move into a class from `none`,
    /// or none at all, is tighter. Classes have no order of their own (which is stricter
    /// depends on the modes), so any move out of one is a loosening.
    IntoClass,
    NotOrdered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Apply {
    Toml,
    Model,
    Thinking,
    Subagent,
    Rule,
    /// The owner's goals, `<state>/GOALS.md` (`memory::goal`).
    Goal,
    /// The owner's persona, `<state>/PERSONA.md` (`memory::persona`).
    Persona,
    /// Private facts (approvals spec A1).
    Facts,
    /// `approvals.preset`: the preset and the hand-set values it drops, in one update
    /// (`approvals::preset_edits`); its time limit goes back to the whole set.
    Preset,
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

pub const READ_ONLY: &str = "Read-only";

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
pub const CHANNELS: &[&str] = &[
    "tui", "chat", "discord", "imessage", "telegram", "signal", "web", "passkey",
];

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
        "mcp.<name>.hosts",
        "Connectors",
        "Connector hosts",
        "Exact HOST[:PORT] this connector's secrets are sent to, comma separated. No wildcards.",
        Kind::Text { max: 1000 },
        Access::Propose,
        true,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "mcp.<name>.secrets",
        "Connectors",
        "Connector secrets",
        "surrogate: the connector holds stand-ins and zephyr sends the real secret only to its hosts. plain: it holds the real ones.",
        Kind::Choice(&["surrogate", "plain"]),
        Access::Propose,
        true,
        Stricter::Earlier,
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
        "goal",
        "Goals and review",
        "Goals",
        "Goals the agent keeps in mind in every run: /goal add, done, rm.",
        Kind::Text { max: 500 },
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Goal,
    ),
    row(
        "review.at",
        "Goals and review",
        "Daily review",
        "When the daily review of the goals, the day's activity and memory is sent, HH:MM in \
         the time zone; none is off.",
        Kind::Optional(&Kind::Text { max: 5 }),
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "memory.auto",
        "Memory",
        "Save at once",
        "off: every memory change waits for /memory ok; owner: one zephyr saw came only from \
         you is saved at once. Off is the safer choice.",
        Kind::Choice(&["off", "owner"]),
        Access::Tighten,
        true,
        Stricter::Earlier,
        Apply::Toml,
    ),
    row(
        "memory.notify",
        "Memory",
        "Memory notices",
        "each: you are sent each memory change after its run; digest: the daily review lists \
         them instead (needs review.at).",
        Kind::Choice(&["each", "digest"]),
        Access::Tighten,
        false,
        Stricter::Earlier,
        Apply::Toml,
    ),
    row(
        "persona",
        "Memory",
        "Persona",
        "Who the agent is with you, in every run: /persona set, clear.",
        Kind::Text { max: 4000 },
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Persona,
    ),
    row(
        "voice.transcribe",
        "Voice",
        "Transcribe voice notes",
        "Whether a voice note you send is transcribed on this machine for the agent to read.",
        Kind::Bool,
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "voice.reply",
        "Voice",
        "Spoken replies",
        "off: replies are text only; mirror: a reply to a voice note is also spoken; always: \
         every reply on Discord and iMessage is.",
        Kind::Choice(&crate::voice::REPLIES),
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "voice.languages",
        "Voice",
        "Languages",
        "The languages you speak, as whisper's codes, comma separated. A note heard as another \
         is transcribed again as the first.",
        Kind::List(crate::voice::LANGUAGES),
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "voice.max_secs",
        "Voice",
        "Longest voice note",
        "A voice note longer than this many seconds is saved but not transcribed.",
        Kind::Int { min: 10, max: 900 },
        Access::Propose,
        false,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "facts",
        "Approvals",
        "Private facts",
        "Things that must not leave in an outgoing call, kept by name: /facts lists them, \
         /facts rm NAME and /facts allow NAME TARGET loosen the check. Values are saved only \
         in zephyr attach or with `zephyr facts add`. The agent may add one, never change one.",
        Kind::Text { max: 4000 },
        Access::Propose,
        true,
        Stricter::NotOrdered,
        Apply::Facts,
    ),
    row(
        "network.allow",
        "Security",
        "Network exceptions",
        "Exact HOST:PORT the agent may reach beyond public ports 80 and 443, comma separated. No wildcards.",
        Kind::Text { max: 2000 },
        Access::Propose,
        true,
        Stricter::NotOrdered,
        Apply::Toml,
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
    row(
        CLASSES_KEY,
        "Connectors",
        "Connector tool class",
        "Which approvals a connector's tool follows, over what its name implies: messages, \
         posts, money or none; the agent may only move a tool from none into a class.",
        Kind::Choice(&crate::approvals::CLASS_CHOICES),
        Access::Tighten,
        true,
        Stricter::IntoClass,
        Apply::Toml,
    ),
    row(
        "approvals.preset",
        "Approvals",
        "Preset",
        "manual asks before every message, post and payment; semi lets known recipients \
         through and delays posts 5 minutes; full asks for nothing. A preset never loosens a \
         value set by hand.",
        Kind::Choice(&Preset::NAMES),
        Access::Tighten,
        true,
        Stricter::Earlier,
        Apply::Preset,
    ),
    row(
        "approvals.messages",
        "Approvals",
        "Messages",
        "What a message to someone waits for; default follows the preset.",
        Kind::Optional(&Kind::Mode(Class::Messages)),
        Access::Tighten,
        true,
        Stricter::Approval,
        Apply::Toml,
    ),
    row(
        "approvals.posts",
        "Approvals",
        "Posts",
        "What a public post waits for; default follows the preset.",
        Kind::Optional(&Kind::Mode(Class::Posts)),
        Access::Tighten,
        true,
        Stricter::Approval,
        Apply::Toml,
    ),
    row(
        "approvals.money",
        "Approvals",
        "Payments",
        "What a payment waits for; auto N lets one through up to N in the currency; default \
         follows the preset.",
        Kind::Optional(&Kind::Mode(Class::Money)),
        Access::Tighten,
        true,
        Stricter::Approval,
        Apply::Toml,
    ),
    row(
        "approvals.currency",
        "Approvals",
        "Currency",
        "The ISO 4217 currency an auto N cap counts in.",
        Kind::Text { max: 3 },
        Access::Propose,
        // C2: another currency changes what an `auto N` cap lets through.
        true,
        Stricter::NotOrdered,
        Apply::Toml,
    ),
    row(
        "approvals.facts",
        "Approvals",
        "Facts check",
        "What a call holding one of your private facts does: hold it, warn you, or nothing; \
         default follows the preset.",
        Kind::Optional(&Kind::Choice(&FactsMode::NAMES)),
        Access::Tighten,
        true,
        Stricter::Earlier,
        Apply::Toml,
    ),
    row(
        "approvals.strong_only",
        "Approvals",
        "Approved only from a strong channel",
        "money: a payment's /ok only from a strong channel. loosen names the rule that a \
         loosening of approvals or a fact needs one, which holds whatever this says. The agent \
         may only add entries.",
        Kind::List(&crate::approvals::STRONG_ONLY),
        Access::Tighten,
        true,
        Stricter::More,
        Apply::Toml,
    ),
    fixed(
        "owners",
        "Owners",
        "Who may talk to the agent, per channel.",
    ),
    fixed("discord", "Discord", "The Discord adapter."),
    fixed("imessage", "iMessage", "The BlueBubbles adapter."),
    fixed("telegram", "Telegram", "The Telegram bot."),
    fixed("signal", "Signal", "The signal-cli adapter."),
    fixed("attach", "Attach", "The rpi --connect listener."),
    fixed("web", "Web app", "The web app's listener."),
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

/// The entry every `mcp.NAME.classes.TOOL` key shares.
const CLASSES_KEY: &str = "mcp.<name>.classes.<tool>";

/// The setting for a dotted key. `mcp.NAME.enabled` finds the connector entry, and
/// `mcp.NAME.classes.TOOL` the one for a connector tool's class.
pub fn find(key: &str) -> Option<&'static Setting> {
    if classes_key(key).is_some() {
        return SETTINGS.iter().find(|s| s.key == CLASSES_KEY);
    }
    if key.starts_with("mcp.") {
        let (_, field) = connector(key)?;
        return (SETTINGS.iter()).find(|s| s.key.strip_prefix("mcp.<name>.") == Some(field));
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
    if s.key == "approvals.currency" {
        let code = crate::approvals::currency_code(raw);
        return code
            .map(Value::from)
            .map_err(|e| format!("[approvals] {e}"));
    }
    let value = parse_kind(&s.kind, raw.trim(), &named(s.key))?;
    if s.key == "settings.strong_channels" && !list_names(raw).contains(&"tui") {
        return Err("tui stays a strong channel".into());
    }
    if s.key == "network.allow" {
        crate::egress::parse_allow(raw)?;
    }
    if s.key == "mcp.<name>.hosts" {
        for host in raw.split(',') {
            crate::broker::parse_host(host)?;
        }
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
        Kind::Mode(class) => match crate::approvals::parse_mode(*class, raw) {
            Ok(mode) => Ok(Value::from(mode.to_string())),
            Err(_) => Err(format!(
                "{key} is {}, not {raw:?}",
                crate::approvals::takes(*class)
            )),
        },
    }
}

/// The class a mode setting is for.
fn mode_class(kind: &Kind) -> Option<Class> {
    match kind {
        Kind::Mode(class) => Some(*class),
        Kind::Optional(inner) => mode_class(inner),
        _ => None,
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
pub(crate) fn duration(raw: &str) -> Result<Duration, String> {
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
        // Ordered by `approvals::stricter` in `tighter`.
        Kind::Model | Kind::Text { .. } | Kind::Mode(_) => None,
    }
}

/// Whether going from `old` to `new` is no looser than before (`None` is an absent value), or
/// `None` when the setting has no order or a value does not parse. Equal values are not looser.
pub fn tighter(s: &Setting, old: Option<&str>, new: Option<&str>) -> Option<bool> {
    // A clearing word is an absent value only where it clears: `none` is a tool's class.
    let value = |v: &str| !(matches!(s.kind, Kind::Optional(_)) && is_clear(v));
    let old = old.map(str::trim).filter(|o| value(o));
    let new = new.map(str::trim).filter(|n| value(n));
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
    if s.stricter == Stricter::Approval {
        let class = mode_class(&s.kind)?;
        let parse = |raw| crate::approvals::parse_mode(class, raw).ok();
        return Some(!crate::approvals::stricter(parse(old)?, parse(new)?));
    }
    if s.stricter == Stricter::IntoClass {
        let class = |raw: &str| {
            let raw = raw.to_ascii_lowercase();
            crate::approvals::CLASS_CHOICES
                .contains(&raw.as_str())
                .then_some(raw)
        };
        let (old, new) = (class(old)?, class(new)?);
        return Some(old == new || old == "none");
    }
    let (a, b) = (rank(&s.kind, old)?, rank(&s.kind, new)?);
    match (s.stricter, a, b) {
        (Stricter::Lower | Stricter::NoneIsLoosest, Rank::Num(a), Rank::Num(b)) => Some(b <= a),
        (Stricter::Higher, Rank::Num(a), Rank::Num(b)) => Some(b >= a),
        (Stricter::Earlier, Rank::Index(a), Rank::Index(b)) => Some(b <= a),
        (Stricter::FalseIsStricter, Rank::Flag(a), Rank::Flag(b)) => Some(!b || a),
        (Stricter::Fewer, Rank::Set(a), Rank::Set(b)) => Some(b.iter().all(|n| a.contains(n))),
        (Stricter::More, Rank::Set(a), Rank::Set(b)) => Some(a.iter().all(|n| b.contains(n))),
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

/// What `key` comes to once cleared, as replies show it, where that is not simply unset: an
/// approval class follows the preset.
pub fn cleared(cfg: &Config, key: &str) -> Option<String> {
    crate::approvals::cleared(cfg, key.strip_prefix("approvals.")?)
}

/// The connector and field a `mcp.NAME.enabled`, `.hosts` or `.secrets` key names.
fn connector(key: &str) -> Option<(&str, &str)> {
    let (name, field) = key.strip_prefix("mcp.")?.rsplit_once('.')?;
    let known = matches!(field, "enabled" | "hosts" | "secrets");
    (known && !name.is_empty() && !name.contains('.')).then_some((name, field))
}

/// The connector and tool a `mcp.NAME.classes.TOOL` key names.
fn classes_key(key: &str) -> Option<(&str, &str)> {
    let (name, tool) = key.strip_prefix("mcp.")?.split_once(".classes.")?;
    let plain = |s: &str| !s.is_empty() && !s.contains('.');
    (plain(name) && plain(tool)).then_some((name, tool))
}

/// The `[[mcp]]` table named `name`.
fn mcp_table<'a>(doc: &'a DocumentMut, name: &str) -> Option<&'a Table> {
    let list = doc.get("mcp")?.as_array_of_tables()?;
    list.iter()
        .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
}

fn mcp_table_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> anyhow::Result<&'a mut Table> {
    let list = doc.get_mut("mcp").and_then(Item::as_array_of_tables_mut);
    list.and_then(|l| {
        l.iter_mut()
            .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
    })
    .ok_or_else(|| anyhow::anyhow!("no connector named {name}"))
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
        "review.at" => cfg.review_at().map(String::from),
        "memory.notify" => Some(cfg.memory_notify().into()),
        "memory.auto" => Some(cfg.memory_auto().into()),
        "auth.enabled" => Some(cfg.auth.enabled.to_string()),
        "auth.refresh_minutes" => Some(cfg.auth.refresh_minutes.to_string()),
        "settings.strong_channels" => Some(cfg.settings.strong_channels.join(", ")),
        "network.allow" => Some(cfg.network.allow.clone()).filter(|a| !a.is_empty()),
        "voice.transcribe" => Some(cfg.voice().transcribe.to_string()),
        "voice.reply" => Some(cfg.voice().reply),
        "voice.languages" => Some(cfg.voice().languages.join(", ")),
        "voice.max_secs" => Some(cfg.voice().max_secs.to_string()),
        key if key.starts_with("approvals.") => {
            crate::approvals::value(cfg, &key["approvals.".len()..])
        }
        key => {
            if let Some((name, tool)) = classes_key(key) {
                let m = cfg.mcp.iter().find(|m| m.name == name)?;
                // Without the owner's class, the one the tool's name implies.
                return Some(match m.classes.get(tool) {
                    Some(class) => class.clone(),
                    None => crate::approvals::verb_class(name, tool).into(),
                });
            }
            let (name, field) = connector(key)?;
            let m = cfg.mcp.iter().find(|m| m.name == name)?;
            match field {
                // Where its stand-ins are swapped; none for a connector that holds none.
                "hosts" => {
                    let hosts = match crate::broker::plain(m) {
                        true => Vec::new(),
                        false => crate::broker::connector_hosts(m),
                    };
                    let hosts: Vec<&str> = hosts.iter().map(|h| crate::vault::shown(h)).collect();
                    (!hosts.is_empty()).then(|| hosts.join(", "))
                }
                // What is in effect: the real secrets only when the owner wrote `plain`.
                "secrets" => Some(match crate::broker::plain(m) {
                    true => "plain".into(),
                    false => "surrogate".into(),
                }),
                _ => Some(m.enabled.unwrap_or(true).to_string()),
            }
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
    if let Some((name, tool)) = classes_key(key) {
        let classes = mcp_table(doc, name)?.get("classes")?.as_table_like()?;
        return classes.get(tool)?.as_str().map(String::from);
    }
    let Some((name, field)) = connector(key) else {
        return crate::host::conf::get(doc, key);
    };
    let value = mcp_table(doc, name)?.get(field)?;
    (value.as_bool().map(|b| b.to_string())).or_else(|| value.as_str().map(String::from))
}

/// Sets `key` to `value` in `doc`, or removes it (`None`, back to its default). A value already
/// there keeps its comment.
pub fn write(doc: &mut DocumentMut, key: &str, value: Option<&Value>) -> anyhow::Result<()> {
    if let Some((name, tool)) = classes_key(key) {
        let t = mcp_table_mut(doc, name)?;
        let Some(v) = value else {
            let classes = t.get_mut("classes").and_then(Item::as_table_like_mut);
            if let Some(classes) = classes {
                classes.remove(tool);
                if classes.is_empty() {
                    t.remove("classes");
                }
            }
            return Ok(());
        };
        let classes = t
            .entry("classes")
            .or_insert_with(|| Item::Value(Value::InlineTable(toml_edit::InlineTable::new())));
        let classes = (classes.as_table_like_mut())
            .ok_or_else(|| anyhow::anyhow!("classes of connector {name} is not a table"))?;
        classes.insert(tool, Item::Value(v.clone()));
        return Ok(());
    }
    if let Some((name, field)) = connector(key) {
        let t = mcp_table_mut(doc, name)?;
        match value {
            Some(v) => t[field] = Item::Value(v.clone()),
            None => drop(t.remove(field)),
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
pub(crate) fn quoted(text: &str) -> String {
    quoted_cut(text, REASON_CHARS)
}

/// `quoted`, cut at `max` characters: for text that is not the agent's reason but is still not
/// the daemon's (a message's subject, a command a card shows). R41: it goes between quotes, so
/// a quote inside is escaped as `\"` and a backslash as `\\` (else `\"` would undo the escape):
/// the text can never close its quote and add words that read as the daemon's.
pub(crate) fn quoted_cut(text: &str, max: usize) -> String {
    one_line(text, max)
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

/// Text that is not the daemon's as part of one line, not between quotes (a recipient, a
/// summary): control and direction characters (D1: the marks too) dropped, whitespace (line and
/// paragraph separators too) folded, cut at `max`
/// characters. Applied twice it gives the same text, so a stored line can be shown again.
pub(crate) fn one_line(text: &str, max: usize) -> String {
    let clean: String = (text.chars())
        .map(|c| match c.is_control() || zephyr_policy::tools::turns(c) {
            true => ' ',
            false => c,
        })
        .collect();
    cut(&clean.split_whitespace().collect::<Vec<_>>().join(" "), max)
}

/// The reply to a change: `[zephyr] KEY OLD → NEW`, with ` until HH:MM` when it is timed.
pub fn changed(key: &str, old: Option<&str>, new: Option<&str>, until: Option<&str>) -> String {
    let until = until.map_or(String::new(), |t| format!(" until {t}"));
    format!("[zephyr] {key} {} → {}{until}", shown(old), shown(new))
}

/// True when `reply` is `changed`'s line for `key`: the inverse of the builder, so a caller can
/// tell a change from a refusal, a stale answer or "no setting" (`[zephyr] <words>`). Only the
/// first line counts: the router may append what a reload said.
pub fn is_change(reply: &str, key: &str) -> bool {
    let first = reply.lines().next().unwrap_or_default();
    first
        .strip_prefix("[zephyr] ")
        .and_then(|r| r.strip_prefix(key))
        .is_some_and(|r| r.starts_with(' ') && r.contains(" → "))
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

/// A kind as attached clients read it, bounds and choices included, so a client draws the
/// right editor without its own table.
pub fn kind_json(k: &Kind) -> serde_json::Value {
    use serde_json::json;
    match k {
        Kind::Bool => json!({"type": "bool"}),
        Kind::Int { min, max } => json!({"type": "int", "min": min, "max": max}),
        Kind::Float { min, max } => json!({"type": "float", "min": min, "max": max}),
        Kind::Duration { min_secs, max_secs } => {
            json!({"type": "duration", "min_secs": min_secs, "max_secs": max_secs})
        }
        Kind::Choice(o) => json!({"type": "choice", "options": o}),
        Kind::Model => json!({"type": "model"}),
        Kind::Text { max } => json!({"type": "text", "max": max}),
        Kind::List(o) => json!({"type": "list", "options": o}),
        Kind::Optional(inner) => json!({"type": "optional", "inner": kind_json(inner)}),
        Kind::Mode(class) => json!({
            "type": "mode",
            "class": crate::approvals::class_name(*class),
            "options": crate::approvals::forms(*class),
        }),
    }
}

/// What attached clients call `key`'s row: a connector's row names its connector (and a tool's
/// class row its tool), since such rows share one entry and would otherwise look alike.
pub fn label(s: &Setting, key: &str) -> String {
    if let Some((name, tool)) = classes_key(key) {
        return format!("Connector {name}: {tool}");
    }
    match connector(key) {
        Some((name, field)) => format!("Connector {name} {field}"),
        None => s.label.to_string(),
    }
}

/// The agent's access as attached clients read it; `read-only` for an entry edited in the file.
pub fn access_name(s: &Setting) -> &'static str {
    if s.section == READ_ONLY {
        return "read-only";
    }
    match s.agent {
        Access::Never => "never",
        Access::Propose => "propose",
        Access::Tighten => "tighten",
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
            Kind::Mode(class) => crate::approvals::takes(*class).into(),
        }
    }
    match (s.apply, &s.kind) {
        (Apply::Thinking, _) => "one of the levels the model offers (/thinking lists them)".into(),
        (Apply::Subagent, _) => {
            "/subagent add NAME [PROVIDER/MODEL] PROMPT, model, prompt, rm".into()
        }
        (Apply::Rule, _) => "/rule TEXT, /rule rm N".into(),
        (Apply::Goal, _) => "/goal add TEXT, /goal done N, /goal rm N".into(),
        (Apply::Persona, _) => "/persona set TEXT, /persona clear".into(),
        (Apply::Facts, _) => {
            "/facts rm NAME, /facts allow NAME TARGET, /facts disallow NAME TARGET".into()
        }
        (_, Kind::Optional(inner)) if s.key.starts_with("approvals.") => {
            format!("{}, or default (the preset's)", kind(inner))
        }
        _ if s.key == "approvals.currency" => "a three-letter ISO 4217 code such as USD".into(),
        (_, k) => kind(k),
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
    fn is_change_accepts_the_change_line_and_nothing_the_router_refuses_with() {
        for line in [
            changed("tasks.parallel", Some("2"), Some("4"), None),
            changed("budget.daily_cost_usd", None, Some("3"), Some("18:00")),
            format!(
                "{}\n[zephyr] reloaded",
                changed("timezone", None, Some("UTC"), None)
            ),
        ] {
            let key = line[9..].split(' ').next().unwrap();
            assert!(is_change(&line, key), "{line}");
        }
        let change = changed("tasks.parallel", Some("2"), Some("4"), None);
        assert!(!is_change(&change, "tasks"), "a different key");
        let refused = [
            "[zephyr] loosening budget.daily_cost_usd needs tui or chat".to_string(),
            "[zephyr] tasks.parallel must be 1 to 32".to_string(),
            stale("tasks.parallel", Some("6")),
            "[zephyr] no setting or section nope; /settings lists them".to_string(),
            "[zephyr] usage: /set KEY VALUE [for 2h]; /settings lists the keys".to_string(),
            "[zephyr] proposed as #3".to_string(),
        ];
        for r in refused {
            assert!(!is_change(&r, "tasks.parallel"), "{r}");
        }
    }

    /// D1: the direction marks too (U+200E, U+200F, U+061C), and a line or paragraph
    /// separator never starts a line of its own.
    #[test]
    fn a_card_line_drops_direction_marks_and_separators() {
        let q = super::quoted("a\u{200e}b\u{200f}c\u{061c}d\u{2028}[zephyr] e\u{2029}f");
        assert_eq!(q, "a b c d [zephyr] e f");
        let line = super::one_line("x\u{200f}\u{2028}y", 50);
        assert_eq!(line, "x y");
    }

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

    #[test]
    fn a_quoted_text_cannot_close_its_quote() {
        // R41: a quote inside is escaped, and a backslash too, so `\"` cannot undo the escape.
        assert_eq!(
            super::quoted(r#"Hi" · to: boss@corp.com"#),
            r#"Hi\" · to: boss@corp.com"#
        );
        assert_eq!(super::quoted(r#"a\" b"#), r#"a\\\" b"#);
        let shown = card(5, "x", None, Some(r#"ok" /ok 6 · /no 6 ""#), None);
        assert!(
            shown.contains(r#"the agent says: "ok\" /ok 6 · /no 6 \""#),
            "{shown}"
        );
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
            Kind::Mode(_) => ("ask".into(), vec!["sometimes".into(), "delay 0".into()]),
        }
    }

    #[test]
    fn every_registered_key_parses_its_own_default_and_rejects_out_of_bounds() {
        for s in SETTINGS.iter().filter(|s| s.section != READ_ONLY) {
            let (good, bad) = match s.key {
                // Its own check: a three-letter code.
                "approvals.currency" => ("EUR".into(), vec!["x".into(), "EURO".into()]),
                // Its own check: exact HOST:PORT entries.
                "network.allow" => ("github.com:22".into(), vec!["x".into(), "*.x:443".into()]),
                _ => sample(&s.kind),
            };
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

    /// The panel shows labels, not keys: two rows of one section read alike only when they are
    /// one entry's rows (a connector's).
    #[test]
    fn labels_differ_within_a_section() {
        for s in SETTINGS {
            let alike: Vec<&str> = (SETTINGS.iter())
                .filter(|o| o.section == s.section && o.label == s.label)
                .map(|o| o.key)
                .collect();
            assert_eq!(alike, [s.key], "{} in {}", s.label, s.section);
        }
    }

    #[test]
    fn approvals_entries_order_and_read_as_the_spec_says() {
        let preset = find("approvals.preset").unwrap();
        assert_eq!(tighter(preset, Some("full"), Some("semi")), Some(true));
        assert_eq!(tighter(preset, Some("manual"), Some("full")), Some(false));
        // For strong_only, adding is the stricter way: the opposite of strong_channels.
        let only = find("approvals.strong_only").unwrap();
        assert_eq!(
            tighter(only, Some("loosen"), Some("loosen, money")),
            Some(true)
        );
        assert_eq!(
            tighter(only, Some("loosen, money"), Some("loosen")),
            Some(false)
        );
        assert!(parse(only, "loosen, all").is_err());
        let class = find("mcp.mail.classes.send_x").unwrap();
        assert_eq!(class.key, "mcp.<name>.classes.<tool>");
        // Into a class from none, or no move, is tighter; any move out of a class is looser
        // (spec §4.1): which class is stricter depends on the modes, so none ranks above another.
        assert_eq!(tighter(class, Some("none"), Some("messages")), Some(true));
        assert_eq!(tighter(class, Some("none"), Some("money")), Some(true));
        assert_eq!(tighter(class, Some("posts"), Some("posts")), Some(true));
        assert_eq!(tighter(class, Some("messages"), Some("none")), Some(false));
        for (old, new) in [
            ("messages", "money"),
            ("money", "messages"),
            ("messages", "posts"),
        ] {
            assert_eq!(
                tighter(class, Some(old), Some(new)),
                Some(false),
                "{old} → {new}"
            );
        }
        assert_eq!(tighter(class, Some("none"), Some("spam")), None);
        for bad in ["mcp.mail.classes.", "mcp..classes.x", "mcp.a.classes.b.c"] {
            assert!(find(bad).is_none(), "{bad}");
        }
        for key in [
            "approvals.preset",
            "approvals.messages",
            "approvals.posts",
            "approvals.money",
            "approvals.facts",
            "approvals.strong_only",
            "mcp.mail.classes.x",
        ] {
            let s = find(key).unwrap();
            assert!(s.strong && s.agent == Access::Tighten, "{key}");
        }
        // C2: a stronger currency raises an `auto N` cap's real worth, so a change of currency
        // is a loosening a weak channel cannot make; the agent still only proposes it.
        let cur = find("approvals.currency").unwrap();
        assert!(cur.strong && cur.agent == Access::Propose);
        assert!(loosens(cur, Some("JPY"), Some("USD")));
        let discord = Actor::Owner { channel: "discord" };
        assert_eq!(
            decide(
                cur,
                &discord,
                loosens(cur, Some("JPY"), Some("USD")),
                &strong()
            ),
            Verdict::Refuse("loosening approvals.currency needs tui or chat".into())
        );
        let tui = Actor::Owner { channel: "tui" };
        assert_eq!(decide(cur, &tui, true, &strong()), Verdict::Apply);
        assert_eq!(text(&parse(cur, "eur").unwrap()), "EUR");
        assert!(parse(cur, "euro").is_err() && parse(cur, "E1R").is_err());
        let money = find("approvals.money").unwrap();
        assert_eq!(text(&parse(money, "Auto  20").unwrap()), "auto 20");
        assert_eq!(
            parse(money, "ask-new").unwrap_err(),
            "[approvals] money is ask, delay N (1 to 60 minutes), auto or auto N (a cap), \
             not \"ask-new\""
        );

        let cfg: Config = toml::from_str(
            "rpi = 'a'\nhome = 'b'\n[approvals]\npreset = 'semi'\nmoney = 'auto 20'\n\
             [[mcp]]\nname = 'mail'\ncommand = 'm'\nclasses = { archive = 'posts' }\n",
        )
        .unwrap();
        let v = |key| value(&cfg, key);
        assert_eq!(v("approvals.preset").as_deref(), Some("semi"));
        assert_eq!(v("approvals.posts").as_deref(), Some("delay 5"));
        assert_eq!(v("approvals.money").as_deref(), Some("auto 20"));
        assert_eq!(v("approvals.facts").as_deref(), Some("hold"));
        assert_eq!(v("approvals.currency").as_deref(), Some("USD"));
        assert_eq!(v("approvals.strong_only").as_deref(), Some("loosen"));
        assert_eq!(v("mcp.mail.classes.archive").as_deref(), Some("posts"));
        // Without the owner's class, the verb rule's.
        assert_eq!(v("mcp.mail.classes.send_note").as_deref(), Some("messages"));
        assert_eq!(v("mcp.mail.classes.get_note").as_deref(), Some("none"));
        assert_eq!(v("mcp.nope.classes.x"), None);
        // Cleared, a class follows the preset.
        assert_eq!(cleared(&cfg, "approvals.money").as_deref(), Some("ask"));
        assert_eq!(cleared(&cfg, "approvals.facts").as_deref(), Some("hold"));
        assert_eq!(cleared(&cfg, "budget.daily_tokens"), None);

        let mut doc: DocumentMut = "[[mcp]]\nname = 'mail'\ncommand = 'm'\n".parse().unwrap();
        let none = Value::from("none");
        write(&mut doc, "mcp.mail.classes.send_x", Some(&none)).unwrap();
        assert_eq!(
            read(&doc, "mcp.mail.classes.send_x").as_deref(),
            Some("none")
        );
        assert!(
            doc.to_string().contains("classes = { send_x = \"none\" }"),
            "{doc}"
        );
        let cfg: Config = toml::from_str(&format!("rpi = 'a'\nhome = 'b'\n{doc}")).unwrap();
        assert_eq!(cfg.mcp[0].classes["send_x"], "none");
        write(&mut doc, "mcp.mail.classes.send_x", None).unwrap();
        assert_eq!(read(&doc, "mcp.mail.classes.send_x"), None);
        assert!(write(&mut doc, "mcp.nope.classes.x", Some(&none)).is_err());
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
    fn the_voice_rows_are_the_agents_to_propose_only() {
        let cfg: Config = toml::from_str("rpi = '/r'\nhome = '/h'").unwrap();
        let agent = Actor::Agent { session: "main" };
        for (key, default) in [
            ("voice.transcribe", "true"),
            ("voice.reply", "mirror"),
            ("voice.languages", "ko, en"),
            ("voice.max_secs", "300"),
        ] {
            let s = find(key).unwrap();
            assert_eq!(
                (s.agent, s.strong, s.apply),
                (Access::Propose, false, Apply::Toml)
            );
            assert_eq!(value(&cfg, key).as_deref(), Some(default), "{key}");
            assert!(parse(s, default).is_ok(), "{key}");
            assert_eq!(decide(s, &agent, false, &strong()), Verdict::Propose);
            assert_eq!(decide(s, &agent, true, &strong()), Verdict::Propose);
        }
        assert!(matches!(find("voice.transcribe").unwrap().kind, Kind::Bool));
        let reply = find("voice.reply").unwrap();
        assert!(matches!(reply.kind, Kind::Choice(o) if o == ["off", "mirror", "always"]));
        assert!(parse(reply, "always").is_ok() && parse(reply, "loud").is_err());
        let langs = find("voice.languages").unwrap();
        assert!(parse(langs, "en,ja").is_ok() && parse(langs, "klingon").is_err());
        let secs = find("voice.max_secs").unwrap();
        assert!(matches!(secs.kind, Kind::Int { min: 10, max: 900 }));
        let cfg: Config =
            toml::from_str("rpi = '/r'\nhome = '/h'\n[voice]\nreply = 'off'\nlanguages = ['en']")
                .unwrap();
        assert_eq!(value(&cfg, "voice.reply").as_deref(), Some("off"));
        assert_eq!(value(&cfg, "voice.languages").as_deref(), Some("en"));
    }

    #[test]
    fn network_allow_is_a_strong_proposal_only_row() {
        let s = find("network.allow").unwrap();
        assert_eq!(s.agent, Access::Propose);
        assert!(s.strong);
        let agent = Actor::Agent { session: "main" };
        assert_eq!(decide(s, &agent, false, &strong()), Verdict::Propose);
        assert_eq!(decide(s, &agent, true, &strong()), Verdict::Propose);
        assert!(parse(s, "imap.gmail.com:993, github.com:22").is_ok());
        assert!(parse(s, "*.gmail.com:993").is_err());
        let cfg: Config =
            toml::from_str("rpi = 'a'\nhome = 'b'\n[network]\nallow = 'github.com:22'").unwrap();
        assert_eq!(
            value(&cfg, "network.allow").as_deref(),
            Some("github.com:22")
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
    fn the_web_table_is_read_only() {
        let web = find("web").expect("listed");
        assert_eq!((web.section, web.agent), (READ_ONLY, Access::Never));
        let strong = Actor::Owner { channel: "tui" };
        let refused = decide(web, &strong, false, &["tui".to_string()]);
        assert!(matches!(refused, Verdict::Refuse(_)), "{refused:?}");
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
            Some("tui, chat, passkey")
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

    #[test]
    fn connector_hosts_and_secrets_are_strong_proposal_rows() {
        for (key, row) in [
            ("mcp.notes.hosts", "mcp.<name>.hosts"),
            ("mcp.notes.secrets", "mcp.<name>.secrets"),
        ] {
            let s = find(key).unwrap();
            assert_eq!(s.key, row);
            assert!(s.strong && matches!(s.agent, Access::Propose), "{key}");
        }
        assert!(find("mcp.notes.nope").is_none() && find("mcp..hosts").is_none());
        assert_eq!(
            label(find("mcp.notes.hosts").unwrap(), "mcp.notes.hosts"),
            "Connector notes hosts"
        );
        let hosts = find("mcp.notes.hosts").unwrap();
        assert!(parse(hosts, "api.x.test, x.test:8443").is_ok());
        for bad in ["*.x.test", "https://x.test", "1.2.3.4", "x.test,,y.test"] {
            assert!(parse(hosts, bad).is_err(), "{bad}");
        }
        let secrets = find("mcp.notes.secrets").unwrap();
        assert!(parse(secrets, "plain").is_ok() && parse(secrets, "raw").is_err());
        // `plain` hands the secret over: it is the looser of the two.
        assert_eq!(
            tighter(secrets, Some("plain"), Some("surrogate")),
            Some(true)
        );
        assert!(loosens(secrets, Some("surrogate"), Some("plain")));
        assert!(loosens(secrets, None, Some("plain")));
        assert!(loosens(hosts, Some("a.test"), Some("b.test")));
    }

    #[test]
    fn connector_hosts_and_secrets_read_and_write_their_table() {
        let text = "[[mcp]]\nname = \"notes\"\nurl = \"https://n.test/mcp\"\n\
                    [[mcp]]\nname = \"local\"\ncommand = \"srv\"\n";
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = '/h'\n{text}")).unwrap();
        // Plain is never implied: a connector holds stand-ins until the owner says otherwise,
        // and a remote one's would go to its own URL's host.
        assert_eq!(value(&cfg, "mcp.notes.hosts").as_deref(), Some("n.test"));
        assert_eq!(
            value(&cfg, "mcp.notes.secrets").as_deref(),
            Some("surrogate")
        );
        assert_eq!(value(&cfg, "mcp.local.hosts"), None);
        assert_eq!(
            value(&cfg, "mcp.local.secrets").as_deref(),
            Some("surrogate")
        );
        let mut doc: DocumentMut = text.parse().unwrap();
        assert_eq!(read(&doc, "mcp.local.hosts"), None);
        write(
            &mut doc,
            "mcp.local.hosts",
            Some(&Value::from("api.l.test")),
        )
        .unwrap();
        write(&mut doc, "mcp.local.secrets", Some(&Value::from("plain"))).unwrap();
        assert_eq!(read(&doc, "mcp.local.hosts").as_deref(), Some("api.l.test"));
        assert_eq!(read(&doc, "mcp.local.secrets").as_deref(), Some("plain"));
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = '/h'\n{doc}")).unwrap();
        cfg.validate().unwrap();
        assert_eq!(cfg.mcp[1].hosts, "api.l.test");
        write(&mut doc, "mcp.local.hosts", None).unwrap();
        assert_eq!(read(&doc, "mcp.local.hosts"), None);
        assert!(write(&mut doc, "mcp.nope.hosts", Some(&Value::from("x.test"))).is_err());
    }
}
