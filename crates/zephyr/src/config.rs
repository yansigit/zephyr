//! `zephyr.toml`: where rpi lives, where state lives, and who may talk to the agent.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The pinned `rpi` binary.
    pub rpi: PathBuf,
    /// The agent's home and the child's cwd: `sessions/`, `memory/`, `accounts/`, `AGENTS.md`.
    pub home: PathBuf,
    /// The daemon's own files: `state.db`, `secrets.env`, `zephyr.sock`, `zephyr.lock`.
    /// Default: `home`. In the container `/var/lib/zephyr`, which the agent cannot enter
    /// (DESIGN §4.4).
    pub state: Option<PathBuf>,
    /// rpi agent dir (`RPI_CODING_AGENT_DIR`): its models, credentials and settings.
    pub agent_dir: Option<PathBuf>,
    /// `--model` for the child, e.g. `anthropic/claude-sonnet-5`. Unused when `[[account]]` is set.
    pub model: Option<String>,
    /// `[[account]]`: the account pool, in failover order (DESIGN §4.9). Empty means one
    /// child on `agent_dir` and `model`.
    #[serde(default, rename = "account")]
    pub accounts: Vec<Account>,
    /// The `zephyr-policy` cdylib, loaded into every child with `--extension`. When set, a
    /// child that does not list the `zephyr-policy` command (rpi skips a plugin that fails
    /// to load) is refused and gets no prompts.
    pub policy_plugin: Option<PathBuf>,
    /// More extensions for every child, each passed as `--extension` after `policy_plugin`.
    #[serde(default)]
    pub extensions: Vec<PathBuf>,
    /// The Discord adapter. Its owner list is `owners.discord`. Absent means no Discord.
    pub discord: Option<crate::discord::Config>,
    /// The BlueBubbles iMessage adapter. Its owner list is `owners.imessage` (handles). Absent means no iMessage.
    pub imessage: Option<crate::imessage::Config>,
    /// The Telegram bot. Its owner list is `owners.telegram` (user ids). Absent means no Telegram.
    pub telegram: Option<crate::telegram::Config>,
    /// The signal-cli adapter. Its owner list is `owners.signal` (ACIs). Absent means no Signal.
    pub signal: Option<crate::signal::Config>,
    /// Owner allowlist per channel name. Senders not listed are dropped.
    #[serde(default)]
    pub owners: HashMap<String, Vec<String>>,
    /// The `rpi --connect` attach listener (DESIGN §4.3). Off when absent.
    pub attach: Option<Attach>,
    /// The web app's listener (DESIGN §4.3.3). Off when absent.
    pub web: Option<Web>,
    /// Daily limits on rpi's reported usage (DESIGN §4.4). Off when absent.
    pub budget: Option<Budget>,
    /// `[tasks]`: how many background tasks run at once (DESIGN §4.12). Default 2 when absent.
    pub tasks: Option<Tasks>,
    /// `[[subagent]]`: the owner's named helpers the agent starts tasks as (DESIGN §4.12).
    #[serde(default, rename = "subagent")]
    pub subagents: Vec<Subagent>,
    /// IANA time zone for job cron schedules, e.g. `Asia/Seoul`. Default: the system's.
    pub timezone: Option<String>,
    /// Wall clock cap for one job run. Not configurable; tests shorten it.
    #[serde(skip, default = "job_timeout")]
    pub job_timeout: std::time::Duration,
    /// How long an `exec` holds the agent's turn before it moves to the background (DESIGN
    /// §4.13). Not configurable; tests shorten it.
    #[serde(skip, default = "exec_backstop")]
    pub exec_backstop: std::time::Duration,
    /// A run with no rpi event for this long is stuck and aborted (DESIGN §4.4); twice this while
    /// a tool runs, since rpi reports nothing between a silent command's start and end. Not
    /// configurable; tests shorten it.
    #[serde(skip, default = "stall_limit")]
    pub stall_limit: std::time::Duration,
    /// The router's housekeeping tick (watchdog, run caps, sign-in notices, config edits) and how
    /// often a run's progress is shown again. Not configurable; tests shorten it.
    #[serde(skip, default = "tick")]
    pub tick: std::time::Duration,
    /// The shortest time limit a setting change may carry (`for 30s`). Not configurable; tests
    /// shorten it.
    #[serde(skip, default = "min_until")]
    pub min_until: std::time::Duration,
    /// Origins a Web Push subscription may also name, over `http` too (DESIGN §4.3.3). Not
    /// configurable, and empty; tests point it at a fake push service.
    #[serde(skip)]
    pub push_hosts: Vec<String>,
    /// Lets the web app start outside the network floor. Not configurable, and off; the tests'
    /// process is not a container, and sets it.
    #[serde(skip)]
    pub web_off_floor: bool,
    /// How long five failed passkey attempts close the web app's sign-in. Not configurable;
    /// tests shorten it.
    #[serde(skip, default = "web_lockout")]
    pub web_lockout: std::time::Duration,
    /// How often a web socket is pinged; one silent for twice this is closed. Not configurable;
    /// tests shorten it.
    #[serde(skip, default = "web_ping")]
    pub web_ping: std::time::Duration,
    /// `[agent]`: what the agent's processes get (DESIGN §4.4).
    #[serde(default)]
    pub agent: Agent,
    /// OAuth refresh (DESIGN §4.7). Defaults apply when the table is absent.
    #[serde(default)]
    pub auth: crate::auth_keeper::AuthConfig,
    /// `[[mcp]]`: the connectors (MCP servers) the agent may call (DESIGN §4.6).
    #[serde(default, rename = "mcp")]
    pub mcp: Vec<Mcp>,
    /// `[settings]`: who may loosen a protected setting (settings spec §2).
    #[serde(default)]
    pub settings: Settings,
    /// `[network]`: where the agent may connect beyond public ports 80 and 443 (DESIGN §4.20).
    #[serde(default)]
    pub network: Network,
    /// `[memory]`: how memory changes are told (DESIGN §4.6). `each` when absent.
    pub memory: Option<MemorySettings>,
    /// `[review]`: the built-in daily review (DESIGN §4.5). Off when absent.
    pub review: Option<Review>,
    /// `[approvals]`: how messages, posts and payments wait for the owner (approvals spec §4.3).
    /// Absent is the `manual` preset. `approvals::effective` reads it.
    pub approvals: Option<Approvals>,
    /// `[voice]`: voice notes in and spoken replies out (DESIGN §4.3). `Config::voice` reads it.
    pub voice: Option<Voice>,
    /// `[browser]`: the agent's browser pacing (DESIGN §4.24). Absent unless the owner set a key:
    /// `Config::pace_ms` reads the default.
    pub browser: Option<Browser>,
    /// `[cards]`: virtual cards (DESIGN §4.25). Absent, or with no provider: off.
    pub cards: Option<Cards>,
    /// Lets `[cards] provider = "fake"` name the scripted test issuer. Not configurable.
    #[serde(skip)]
    pub cards_fake: bool,
    /// Link's API and sign-in base URLs. Not configurable; tests point them at a stub.
    #[serde(skip, default = "link_api")]
    pub link_api: String,
    #[serde(skip, default = "link_auth")]
    pub link_auth: String,
    /// `[calls]`: phone calls (DESIGN §4.26). Off without a `provider`. `Config::calls` reads it.
    pub calls: Option<Calls>,
    /// Lets the calls listener run outside the network floor. Not configurable, and off; the
    /// tests' process is not a container, and sets it.
    #[serde(skip)]
    pub calls_off_floor: bool,
    /// How long a dialled call may go unanswered: Telnyx rings for 30 s. Not configurable; tests
    /// shorten it.
    #[serde(skip, default = "calls_ring")]
    pub calls_ring: std::time::Duration,
    /// A call on which nobody has spoken for this long is ended. Not configurable; tests shorten it.
    #[serde(skip, default = "calls_silence")]
    pub calls_silence: std::time::Duration,
    /// The clock the calling hours are read from. Not configurable, and the real one; tests fix it.
    #[serde(skip)]
    pub calls_now: Option<i64>,
    /// What a call's cap is counted in: a minute. Not configurable; tests shorten it.
    #[serde(skip, default = "calls_minute")]
    pub calls_minute: std::time::Duration,
    /// The image's voice folder (`voice::DIR`): ffmpeg, whisper-cli, Piper, models and voices.
    /// Not configurable; tests point it at small fakes.
    #[serde(skip, default = "voice_dir")]
    pub voice_dir: PathBuf,
    /// How long one whisper run may take before it is killed. Not configurable; tests shorten it.
    #[serde(skip, default = "voice_wait")]
    pub voice_wait: std::time::Duration,
    /// Lets voice run outside the network floor. Not configurable, and off; the tests' process
    /// is not a container, and sets it.
    #[serde(skip)]
    pub voice_off_floor: bool,
    /// The file this was loaded from, which a reload reads again. `None` when built in memory.
    #[serde(skip)]
    pub path: Option<PathBuf>,
    /// The projects folder `/rollback` checkpoints (DESIGN §4.14): `$ZEPHYR_PROJECTS`, which
    /// `zephyr up` sets. `None` turns checkpoints off. Not configurable; tests set it.
    #[serde(skip, default = "projects")]
    pub projects: Option<PathBuf>,
    /// The real agent-browser, which the daemon's own browser calls (the vault's fill) run by
    /// full path and the browser wrapper is pointed at (approvals spec §4.4). Not configurable;
    /// tests set a fake.
    #[serde(skip, default = "agent_browser")]
    pub agent_browser: PathBuf,
    /// The connector catalog (DESIGN §4.22): the image's `/opt/zephyr/connectors`. Not
    /// configurable; tests point it at their own folder.
    #[serde(skip, default = "catalog_dir")]
    pub catalog_dir: PathBuf,
    /// The image's starter skills (DESIGN §4.23), seeded at start: `/opt/zephyr/skills`. Not
    /// configurable; tests point it at their own folder.
    #[serde(skip, default = "starter_skills")]
    pub starter_skills: PathBuf,
    /// This binary, which a command's output goes through on its way to the log so that no
    /// vault value lands there (`zephyr __scrublog`, DESIGN §4.13). `None` writes the log
    /// directly; the daemon sets it at start, and tests of other things leave it out.
    #[serde(skip)]
    pub scrubber: Option<PathBuf>,
    /// Whether the agent runs under the network floor (DESIGN §4.20): `ZEPHYR_NETWORK=bridge`.
    /// An OAuth sign-in then takes the address its callback could not reach. Not configurable;
    /// tests set it.
    #[serde(skip, default = "floor")]
    pub floor: bool,
    /// The Bot API's base URL. Not configurable; tests point it at a stub.
    #[serde(skip, default = "telegram_api")]
    pub telegram_api: String,
    /// The signal-cli the daemon runs as root, by full path. Not configurable; tests set a fake.
    #[serde(skip, default = "signal_cli")]
    pub signal_cli: PathBuf,
    /// How long the gate lets a browser or desktop command wait while the owner holds the
    /// desktop (DESIGN §4.11). Not configurable; tests shorten it.
    #[serde(skip, default = "desktop_hold")]
    pub desktop_hold: std::time::Duration,
    /// The agent's display, which the owner may take over from the web app (DESIGN §4.3.3):
    /// `$ZEPHYR_DESKTOP`. `None` is no desktop, so no takeover. Not configurable; tests set it.
    #[serde(skip, default = "desktop")]
    pub desktop: Option<String>,
    /// x11vnc, by full path, started for each takeover. Not configurable; tests set a fake.
    #[serde(skip, default = "x11vnc")]
    pub x11vnc: PathBuf,
    /// A takeover ends by itself after this long with no key or pointer from the owner, after
    /// `desktop_cap` in all, and `desktop_gone` after its socket closed. Not configurable (no
    /// registry rows until the owner asks for one); tests shorten them.
    #[serde(skip, default = "desktop_idle")]
    pub desktop_idle: std::time::Duration,
    #[serde(skip, default = "desktop_cap")]
    pub desktop_cap: std::time::Duration,
    #[serde(skip, default = "desktop_gone")]
    pub desktop_gone: std::time::Duration,
    /// How long a take waits for the agent's browser and desktop commands already running to
    /// end, before it kills them (DESIGN §4.11). Not configurable; tests shorten it.
    #[serde(skip, default = "desktop_drain")]
    pub desktop_drain: std::time::Duration,
}

fn link_api() -> String {
    crate::cards_link::API.into()
}

fn link_auth() -> String {
    crate::cards_link::AUTH.into()
}

fn calls_ring() -> std::time::Duration {
    std::time::Duration::from_secs(45)
}

fn calls_silence() -> std::time::Duration {
    std::time::Duration::from_secs(20)
}

fn calls_minute() -> std::time::Duration {
    std::time::Duration::from_secs(60)
}

fn telegram_api() -> String {
    "https://api.telegram.org".into()
}

fn signal_cli() -> PathBuf {
    PathBuf::from("/opt/zephyr/signal/bin/signal-cli")
}

fn voice_dir() -> PathBuf {
    PathBuf::from(crate::voice::DIR)
}

fn voice_wait() -> std::time::Duration {
    std::time::Duration::from_secs(3 * 60)
}

fn web_lockout() -> std::time::Duration {
    std::time::Duration::from_secs(15 * 60)
}

fn web_ping() -> std::time::Duration {
    std::time::Duration::from_secs(30)
}

fn web_port() -> u16 {
    7793
}

fn job_timeout() -> std::time::Duration {
    crate::scheduler::JOB_TIMEOUT
}

fn exec_backstop() -> std::time::Duration {
    std::time::Duration::from_secs(10 * 60)
}

fn stall_limit() -> std::time::Duration {
    std::time::Duration::from_secs(10 * 60)
}

fn tick() -> std::time::Duration {
    std::time::Duration::from_secs(5)
}

fn min_until() -> std::time::Duration {
    std::time::Duration::from_secs(30)
}

fn desktop_hold() -> std::time::Duration {
    std::time::Duration::from_secs(2 * 60)
}

fn desktop() -> Option<String> {
    std::env::var("ZEPHYR_DESKTOP")
        .ok()
        .filter(|d| !d.is_empty())
}

fn x11vnc() -> PathBuf {
    PathBuf::from("/usr/bin/x11vnc")
}

// shortcut: constants, with no registry rows until the owner asks for one.
fn desktop_idle() -> std::time::Duration {
    std::time::Duration::from_secs(10 * 60)
}

fn desktop_cap() -> std::time::Duration {
    std::time::Duration::from_secs(60 * 60)
}

fn desktop_drain() -> std::time::Duration {
    std::time::Duration::from_secs(15)
}

fn desktop_gone() -> std::time::Duration {
    std::time::Duration::from_secs(60)
}

fn floor() -> bool {
    crate::egress::bridge()
}

fn catalog_dir() -> PathBuf {
    PathBuf::from(crate::catalog::DIR)
}

fn starter_skills() -> PathBuf {
    PathBuf::from(crate::drafts::STARTER)
}

fn agent_browser() -> PathBuf {
    PathBuf::from(crate::browser_gate::REAL)
}

fn projects() -> Option<PathBuf> {
    std::env::var_os("ZEPHYR_PROJECTS")
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
}

/// Sent once, when a budget limit is crossed.
pub const BUDGET_NOTICE: &str =
    "[zephyr] daily budget reached: paused until the next UTC day or /resume";

/// `[budget]`: the daily limits are counted per UTC day; crossing one pauses the gateway until
/// the next UTC day or `/resume`. The run limits stop one run (DESIGN §4.4).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub daily_tokens: Option<i64>,
    pub daily_cost_usd: Option<f64>,
    /// Model turns one run may take, main session or job.
    pub run_turns: Option<u32>,
    /// Minutes one main-session run may take. Jobs have their own cap (`JOB_TIMEOUT`).
    pub run_minutes: Option<f64>,
}

impl Budget {
    pub fn exceeded(&self, tokens: i64, cost: f64) -> bool {
        self.daily_tokens.is_some_and(|t| tokens >= t)
            || self.daily_cost_usd.is_some_and(|c| cost >= c)
    }

    /// Why a run that has taken `turns` turns in `elapsed` must stop, if it must.
    pub fn run_over(&self, turns: u32, elapsed: std::time::Duration) -> Option<String> {
        if let Some(max) = self.run_turns.filter(|max| turns >= *max) {
            return Some(format!("{max} turns (budget.run_turns)"));
        }
        self.run_minutes
            .filter(|max| elapsed.as_secs_f64() >= max * 60.0)
            .map(|max| format!("{max} minutes (budget.run_minutes)"))
    }
}

/// `[review]` (DESIGN §4.5): `at`, `HH:MM` in `timezone`, the time of the daily review.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Review {
    pub at: Option<String>,
}

/// `[memory]` (DESIGN §4.6): `notify`, `each` (a notice after each run) or `digest` (the daily
/// review lists the changes).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemorySettings {
    pub notify: Option<String>,
    /// `off` (absent): every change waits for `/memory ok`; `owner`: one the daemon saw came
    /// only from the owner is saved at once (DESIGN §4.6).
    pub auto: Option<String>,
}

/// `HH:MM` as `(hour, minute)`, 00:00 to 23:59, two digits each.
pub fn hh_mm(at: &str) -> Option<(u32, u32)> {
    let (h, m) = at.split_once(':')?;
    let two = |s: &str| -> Option<u32> {
        let digits = s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit());
        digits.then(|| s.parse().ok()).flatten()
    };
    let (h, m) = (two(h)?, two(m)?);
    (h < 24 && m < 60).then_some((h, m))
}

/// `[voice]` (DESIGN §4.3).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Voice {
    /// Whether an owner's voice note is transcribed.
    pub transcribe: bool,
    /// `off`, `mirror` (a reply to a voice note is also spoken) or `always`.
    pub reply: String,
    /// whisper's language codes. A note detected as another is run again as the first; with
    /// one entry nothing is detected.
    pub languages: Vec<String>,
    /// A longer note is not transcribed, 10 to 900.
    pub max_secs: u64,
}

impl Default for Voice {
    fn default() -> Self {
        Self {
            transcribe: true,
            reply: "mirror".into(),
            languages: vec!["ko".into(), "en".into()],
            max_secs: 300,
        }
    }
}

/// `[browser]` (DESIGN §4.24).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Browser {
    /// The least time between two of the agent's navigations or clicks on one site, in ms;
    /// 0 is off.
    pub pace_ms: Option<u64>,
}

/// `browser.pace_ms` when the owner set none.
pub const PACE_MS: u64 = 1500;

/// `[calls]` (DESIGN §4.26).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct Calls {
    /// `telnyx`; absent, calls are off.
    pub provider: Option<String>,
    /// The port Telnyx's media connection and webhooks come in on, behind the owner's
    /// `tailscale funnel` (DESIGN §4.20).
    pub port: Option<u16>,
    /// That listener's public address as Telnyx reaches it: `wss://host[:port]`.
    pub media_url: Option<String>,
    /// Who transcribes and speaks: `provider` (Telnyx, the default) or `local` (whisper and
    /// Piper in the image). Either way the text passes through the daemon.
    pub speech: String,
    /// Whether toll-free numbers may be called. Off: they are shared with the US and the
    /// Caribbean, and many are crisis or health lines.
    pub toll_free: bool,
    /// A call is ended after this many minutes, 1 to 20.
    pub max_minutes: u64,
    /// Who the opening line says the call is for.
    pub caller_name: Option<String>,
    /// Where the other party can reach the owner (CRTC ADAD rule 4(d)): a number, and an email
    /// or a postal address.
    pub contact_phone: Option<String>,
    pub contact_email: Option<String>,
    pub contact_address: Option<String>,
    /// `provider/model` for the call's own agent; `None` follows `[tasks] model`, then the account.
    pub model: Option<String>,
    /// A transcript is deleted after this many days, 1 to 90.
    pub keep_days: u64,
}

impl Default for Calls {
    fn default() -> Self {
        Self {
            provider: None,
            port: None,
            media_url: None,
            speech: "provider".into(),
            toll_free: false,
            max_minutes: 5,
            caller_name: None,
            contact_phone: None,
            contact_email: None,
            contact_address: None,
            model: None,
            keep_days: 30,
        }
    }
}

/// `[tasks]` (DESIGN §4.12).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Tasks {
    /// Tasks that run at once, 1 to 32; more wait queued (up to `store::MAX_OPEN_TASKS` in all).
    #[serde(default = "default_parallel")]
    pub parallel: usize,
    /// `provider/model` for a task that names none, nor a subagent with one; `None` is the
    /// account's own model.
    pub model: Option<String>,
}

/// One `[[subagent]]` (DESIGN §4.12): a task started as `name` gets `prompt` before its goal
/// and runs on `model` unless the agent names one.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Subagent {
    pub name: String,
    pub model: Option<String>,
    pub prompt: String,
}

/// The longest subagent prompt, as long as a task's goal may be.
pub const MAX_SUBAGENT_PROMPT: usize = zephyr_policy::tools::MAX_GOAL;

fn default_parallel() -> usize {
    2
}

/// `[network]` (DESIGN §4.20).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    /// Exact `HOST:PORT` entries, comma separated (`egress::parse_allow`).
    #[serde(default)]
    pub allow: String,
}

/// `[settings]` (settings spec §2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    /// Channels that may loosen a setting marked strong (a budget, this list). The TUI is
    /// always one; `chat` is the root-only socket the host CLI uses.
    #[serde(default = "strong_default")]
    pub strong_channels: Vec<String>,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            strong_channels: strong_default(),
        }
    }
}

fn strong_default() -> Vec<String> {
    vec!["tui".into(), "chat".into(), "passkey".into()]
}

/// `[approvals]` (approvals spec §4.3), kept as the file says it: `Config::validate` refuses a
/// value that does not parse, and `approvals::effective` reads anything that still does not as
/// the strictest. A class left out follows the preset.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approvals {
    /// `manual`, `semi` or `full`; absent is `manual`.
    pub preset: Option<String>,
    /// `ask`, `ask-new`, `delay N` or `auto`.
    pub messages: Option<String>,
    pub posts: Option<String>,
    /// `ask`, `delay N`, `auto` or `auto N` (a cap per purchase, in `currency`).
    pub money: Option<String>,
    /// ISO 4217; absent is `USD`.
    pub currency: Option<String>,
    /// Private facts: `hold`, `warn` or `off`.
    pub facts: Option<String>,
    /// What only a strong channel may approve: `loosen`, `money`. Absent is `["loosen"]`.
    pub strong_only: Option<Vec<String>>,
    /// `[approvals.window]`: the limit over all payments (`approvals::window`).
    pub window: Option<Window>,
}

/// `[approvals.window]` (DESIGN §4.25): past `effects` payments or `amount` (in
/// `approvals.currency`) in `seconds`, a payment waits for a strong channel whatever the mode.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// 1 to 100; absent is 5.
    pub effects: Option<u32>,
    /// Absent is the `auto N` cap when there is one, else no amount limit.
    pub amount: Option<f64>,
    /// 60 to 86 400; absent is 600.
    pub seconds: Option<u32>,
}

/// `[cards]` (DESIGN §4.25).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cards {
    /// `manual` or `link`; absent is off.
    pub provider: Option<String>,
    /// The most one card may be for, in `approvals.currency`; absent is 100.
    pub max: Option<f64>,
    /// How long an issued card stays open, 1 to 60; absent is 15.
    pub minutes: Option<u32>,
}

/// `[agent]`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Agent {
    /// secrets.env variables the agent's processes may see, e.g. an API key a provider reads
    /// from the environment. Every other secrets.env variable is removed from them.
    #[serde(default)]
    pub env: Vec<String>,
}

/// One `[[account]]`. Its credentials live in `<home>/accounts/<name>/`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Account {
    pub name: String,
    /// `<provider>/<model>`; the provider id is what AuthKeeper refreshes.
    pub model: String,
}

impl Account {
    pub fn provider(&self) -> &str {
        self.model.split('/').next().unwrap_or_default()
    }
}

/// One `[[mcp]]`: an MCP server, run by `rpi-mcp-adapter` from the file `drafts::prepare` writes.
/// Either `url` (and `headers`) or `command` (and `args`, `cwd`). Any string value may say
/// `${NAME}` for a secrets.env key.
#[derive(Debug, Clone, Default, PartialEq, Deserialize, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub struct Mcp {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// Seconds per call; the adapter's default is 60.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// The owner's class for a tool, over the verb rule (approvals spec §4.1): `messages`,
    /// `posts`, `money` or `none`. The daemon's, never the adapter's: `drafts::filled` leaves it
    /// out of `mcp.json`, and a draft cannot carry it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub classes: BTreeMap<String, String>,
    /// The exact `HOST[:PORT]`s this connector's secrets are sent to, comma separated (DESIGN
    /// §4.21). The daemon's: left out of `mcp.json`.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub hosts: String,
    /// `surrogate` (the default): the connector holds stand-ins. `plain`: the real secrets.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secrets: Option<String>,
}

impl Mcp {
    /// A name the adapter takes and a folder could hold, and exactly one of `url` or `command`.
    pub fn check(&self) -> anyhow::Result<()> {
        if !crate::drafts::valid_name(&self.name) {
            anyhow::bail!(
                "connector name {:?}: use 1-64 lowercase letters, digits or '-'",
                self.name
            );
        }
        let blank = |v: &Option<String>| v.as_deref().is_none_or(|s| s.trim().is_empty());
        if blank(&self.url) == blank(&self.command) {
            anyhow::bail!("connector {:?}: set either url or command", self.name);
        }
        for host in self.hosts.split(',').filter(|h| !h.trim().is_empty()) {
            crate::broker::parse_host(host)
                .map_err(|e| anyhow::anyhow!("connector {:?}: hosts: {e}", self.name))?;
        }
        if !matches!(self.secrets.as_deref(), None | Some("surrogate" | "plain")) {
            anyhow::bail!(
                "connector {:?}: secrets is \"surrogate\" or \"plain\"",
                self.name
            );
        }
        Ok(())
    }
}

/// `[attach]`: listens on 127.0.0.1 only. The token comes from exactly one of the two sources.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attach {
    pub port: u16,
    /// Name of an env var holding the token.
    pub token_env: Option<String>,
    /// A file holding the token; must not be readable by group or others.
    pub token_file: Option<PathBuf>,
}

/// `[web]`: the web app, on 127.0.0.1 behind `tailscale serve` (DESIGN §4.3.3).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Web {
    /// The name `tailscale serve` answers on. Host and Origin must match it, and it is the
    /// passkeys' relying party.
    pub host: String,
    #[serde(default = "web_port")]
    pub port: u16,
    /// When set, `Tailscale-User-Login` must be one of these. It turns strangers on a shared
    /// tailnet away; it never signs anyone in.
    #[serde(default)]
    pub tailnet_users: Vec<String>,
}

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
        cfg.path = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// A reloaded file keeps the running daemon's values of the fields no file sets (`tick`,
    /// `stall_limit`, `min_until`, `job_timeout`, `exec_backstop`, `path`, `projects`,
    /// `agent_browser`, `push_hosts`, `web_lockout`, `web_ping`, `voice_dir`, `voice_wait`): a reload changes settings,
    /// never these (R30).
    pub fn keep_hidden(&mut self, running: &Config) {
        self.job_timeout = running.job_timeout;
        self.exec_backstop = running.exec_backstop;
        self.stall_limit = running.stall_limit;
        self.tick = running.tick;
        self.min_until = running.min_until;
        self.path = running.path.clone();
        self.projects = running.projects.clone();
        self.agent_browser = running.agent_browser.clone();
        self.catalog_dir = running.catalog_dir.clone();
        self.starter_skills = running.starter_skills.clone();
        self.scrubber = running.scrubber.clone();
        self.floor = running.floor;
        self.push_hosts = running.push_hosts.clone();
        self.web_off_floor = running.web_off_floor;
        self.web_lockout = running.web_lockout;
        self.web_ping = running.web_ping;
        self.telegram_api = running.telegram_api.clone();
        self.signal_cli = running.signal_cli.clone();
        self.desktop_hold = running.desktop_hold;
        self.desktop = running.desktop.clone();
        self.x11vnc = running.x11vnc.clone();
        self.desktop_idle = running.desktop_idle;
        self.desktop_cap = running.desktop_cap;
        self.desktop_gone = running.desktop_gone;
        self.desktop_drain = running.desktop_drain;
        self.voice_dir = running.voice_dir.clone();
        self.voice_wait = running.voice_wait;
        self.voice_off_floor = running.voice_off_floor;
        self.cards_fake = running.cards_fake;
        self.link_api = running.link_api.clone();
        self.link_auth = running.link_auth.clone();
        self.calls_off_floor = running.calls_off_floor;
        self.calls_ring = running.calls_ring;
        self.calls_silence = running.calls_silence;
        self.calls_now = running.calls_now;
        self.calls_minute = running.calls_minute;
    }

    /// Account names become directory names, so each must be a plain, unique path segment.
    /// Whether memory changes wait for the daily review instead of a notice each.
    pub fn digest(&self) -> bool {
        self.memory_notify() == "digest"
    }

    /// `[memory] auto`, `off` when unset.
    pub fn memory_auto(&self) -> &str {
        (self.memory.as_ref())
            .and_then(|m| m.auto.as_deref())
            .unwrap_or("off")
    }

    /// Whether a memory change the daemon saw came only from the owner is saved at once.
    pub fn auto_save(&self) -> bool {
        self.memory_auto() == "owner"
    }

    /// `[memory] notify`, `each` when unset.
    pub fn memory_notify(&self) -> &str {
        (self.memory.as_ref())
            .and_then(|m| m.notify.as_deref())
            .unwrap_or("each")
    }

    /// `[voice]`, with its defaults when the table or a key is absent.
    pub fn voice(&self) -> Voice {
        self.voice.clone().unwrap_or_default()
    }

    /// `[browser] pace_ms`, [`PACE_MS`] when unset.
    pub fn pace_ms(&self) -> u64 {
        (self.browser.as_ref())
            .and_then(|b| b.pace_ms)
            .unwrap_or(PACE_MS)
    }

    /// `[calls]`, with its defaults when the table or a key is absent.
    pub fn calls(&self) -> Calls {
        self.calls.clone().unwrap_or_default()
    }

    /// The clock the calling hours are read from.
    pub fn calls_clock(&self) -> i64 {
        self.calls_now.unwrap_or_else(crate::now)
    }

    /// `[review] at`, when the daily review is on.
    pub fn review_at(&self) -> Option<&str> {
        self.review.as_ref()?.at.as_deref()
    }

    pub fn validate(&self) -> anyhow::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for a in &self.accounts {
            let safe = !a.name.is_empty()
                && !a.name.starts_with('.')
                && a.name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c));
            if !safe {
                anyhow::bail!("account name {:?}: use letters, digits, '-', '_' or '.', not starting with '.'", a.name);
            }
            if !seen.insert(&a.name) {
                anyhow::bail!("account name {:?} is used twice", a.name);
            }
            if a.provider().is_empty() || !a.model.contains('/') {
                anyhow::bail!("account {:?}: model must be <provider>/<model>", a.name);
            }
        }
        for name in &self.agent.env {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
                anyhow::bail!("agent.env {name:?}: use a variable name (letters, digits, '_')");
            }
        }
        if let Some(at) = self.review_at() {
            if hh_mm(at).is_none() {
                anyhow::bail!("[review] at must be HH:MM, e.g. 21:30");
            }
        }
        if !matches!(self.memory_auto(), "off" | "owner") {
            anyhow::bail!("[memory] auto must be off or owner");
        }
        if !matches!(self.memory_notify(), "each" | "digest") {
            anyhow::bail!("[memory] notify must be each or digest");
        }
        if self.digest() && self.review_at().is_none() {
            anyhow::bail!(
                "[memory] notify = \"digest\" needs [review] at: the digest goes in the daily review"
            );
        }
        let voice = self.voice();
        if !crate::voice::REPLIES.contains(&voice.reply.as_str()) {
            anyhow::bail!("[voice] reply must be off, mirror or always");
        }
        if !(10..=900).contains(&voice.max_secs) {
            anyhow::bail!("[voice] max_secs must be 10 to 900");
        }
        // Each goes into whisper's argv, so only its own codes.
        let known = |l: &String| crate::voice::LANGUAGES.contains(&l.as_str());
        if voice.languages.is_empty() || !voice.languages.iter().all(known) {
            anyhow::bail!(
                "[voice] languages must be whisper's language codes, e.g. [\"ko\", \"en\"]"
            );
        }
        if let Some(c) = &self.cards {
            let known = match c.provider.as_deref() {
                None | Some("manual" | "link") => true,
                Some("fake") => self.cards_fake,
                Some(_) => false,
            };
            if !known {
                anyhow::bail!("[cards] provider must be manual or link");
            }
            if c.max.is_some_and(|m| !(m > 0.0 && m <= 1.0e6)) {
                anyhow::bail!("[cards] max must be more than 0, at most 1000000");
            }
            if c.minutes.is_some_and(|m| !(1..=60).contains(&m)) {
                anyhow::bail!("[cards] minutes must be 1 to 60");
            }
            // A hand-made card has no issuer that asks the owner: its /ok is the strong step.
            let strong = crate::approvals::strong_only(self)
                .iter()
                .any(|s| s == "money");
            if c.provider.as_deref() == Some("manual") && !strong {
                anyhow::bail!("[cards] needs approvals.strong_only to include money");
            }
        }
        let calls = self.calls();
        if calls.provider.as_deref().is_some_and(|p| p != "telnyx") {
            anyhow::bail!("[calls] provider must be telnyx");
        }
        if !crate::calls::SPEECH.contains(&calls.speech.as_str()) {
            anyhow::bail!("[calls] speech must be provider or local");
        }
        if !(1..=20).contains(&calls.max_minutes) {
            anyhow::bail!("[calls] max_minutes must be 1 to 20");
        }
        if !(1..=90).contains(&calls.keep_days) {
            anyhow::bail!("[calls] keep_days must be 1 to 90");
        }
        // Telnyx signs this exact address, so it has no path, query or trailing slash of its own.
        let host = |u: &str| {
            u.strip_prefix("wss://").is_some_and(|h| {
                !h.is_empty()
                    && h.chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-.:".contains(c))
            })
        };
        if calls.media_url.as_deref().is_some_and(|u| !host(u)) {
            anyhow::bail!("[calls] media_url must be wss://HOST[:PORT], with no path");
        }
        let shaped = zephyr_policy::tools::model_shape;
        if let Some(m) = calls.model.as_deref().filter(|m| !shaped(m)) {
            anyhow::bail!("[calls] model {m:?}: use PROVIDER/MODEL");
        }
        if !(1..=32).contains(&self.parallel_tasks()) {
            anyhow::bail!("[tasks] parallel must be 1 to 32");
        }
        if self.pace_ms() > 60_000 {
            anyhow::bail!("[browser] pace_ms must be 0 to 60000");
        }
        let shape = zephyr_policy::tools::model_shape;
        if let Some(m) = self.task_model().filter(|m| !shape(m)) {
            anyhow::bail!("[tasks] model {m:?}: use PROVIDER/MODEL");
        }
        let mut names = std::collections::HashSet::new();
        for s in &self.subagents {
            if !zephyr_policy::tools::subagent_name(&s.name) {
                anyhow::bail!(
                    "subagent name {:?}: use up to 32 lowercase letters, digits, '_' or '-', \
                     starting with a letter or digit",
                    s.name
                );
            }
            if !names.insert(&s.name) {
                anyhow::bail!("subagent name {:?} is used twice", s.name);
            }
            if s.prompt.trim().is_empty() || s.prompt.len() > MAX_SUBAGENT_PROMPT {
                anyhow::bail!(
                    "subagent {}: the prompt must be 1 to {MAX_SUBAGENT_PROMPT} bytes",
                    s.name
                );
            }
            if let Some(m) = s.model.as_deref().filter(|m| !shape(m)) {
                anyhow::bail!("subagent {}: model {m:?}: use PROVIDER/MODEL", s.name);
            }
        }
        let mut names = std::collections::HashSet::new();
        for m in &self.mcp {
            m.check()?;
            if !names.insert(&m.name) {
                anyhow::bail!("connector name {:?} is used twice", m.name);
            }
            let classes = crate::approvals::CLASS_CHOICES;
            if let Some((tool, c)) =
                (m.classes.iter()).find(|(_, c)| !classes.contains(&c.as_str()))
            {
                anyhow::bail!(
                    "connector {}: classes {tool} is {c:?}; use messages, posts, money or none",
                    m.name
                );
            }
        }
        if let Some(a) = &self.approvals {
            crate::approvals::check(a)?;
        }
        let strong = &self.settings.strong_channels;
        if let Some(bad) = strong
            .iter()
            .find(|c| !crate::settings::CHANNELS.contains(&c.as_str()))
        {
            anyhow::bail!(
                "[settings] strong_channels names {bad:?}; allowed: {}",
                crate::settings::CHANNELS.join(", ")
            );
        }
        if !strong.iter().any(|c| c == "tui") {
            anyhow::bail!("[settings] strong_channels must keep tui");
        }
        crate::egress::parse_allow(&self.network.allow)
            .map_err(|e| anyhow::anyhow!("network.allow: {e}"))?;
        if let Some(web) = &self.web {
            // A bare name: it is compared with Host and put in the CSP and the cookie's origin.
            let label = |l: &str| {
                !l.is_empty()
                    && (l.bytes())
                        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
            };
            anyhow::ensure!(
                web.host.split('.').all(label),
                "[web] host is the tailnet name alone, like box.tail1234.ts.net: lowercase, \
                 no scheme, port, path or trailing dot"
            );
        }
        crate::scheduler::zone(self)?;
        Ok(())
    }

    /// Tasks that run at once; more wait queued (up to `store::MAX_OPEN_TASKS` in all).
    pub fn parallel_tasks(&self) -> usize {
        self.tasks
            .as_ref()
            .map_or_else(default_parallel, |t| t.parallel)
    }

    /// `[tasks] model`: what a task runs on when neither it nor its subagent names a model.
    pub fn task_model(&self) -> Option<&str> {
        self.tasks.as_ref().and_then(|t| t.model.as_deref())
    }

    /// `<home>/accounts/<name>`: the account's `RPI_CODING_AGENT_DIR`.
    pub fn account_dir(&self, account: &Account) -> PathBuf {
        self.home.join("accounts").join(&account.name)
    }

    /// `$ZEPHYR_CONFIG`, else `~/agent/zephyr.toml`.
    pub fn default_path() -> PathBuf {
        std::env::var_os("ZEPHYR_CONFIG")
            .map(PathBuf::from)
            .unwrap_or_else(|| {
                PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
                    .join("agent/zephyr.toml")
            })
    }

    pub fn is_owner(&self, channel: &str, sender: &str) -> bool {
        // The attach token, or the web app's session cookie, already proved the sender is the owner.
        crate::attach::attached(channel)
            || self
                .owners
                .get(channel)
                .is_some_and(|list| list.iter().any(|o| o == sender))
    }

    pub fn state_dir(&self) -> &Path {
        self.state.as_deref().unwrap_or(&self.home)
    }

    pub fn db_path(&self) -> PathBuf {
        self.state_dir().join("state.db")
    }

    pub fn secrets_path(&self) -> PathBuf {
        self.state_dir().join("secrets.env")
    }

    pub fn session_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.state_dir().join("zephyr.sock")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cards_with_manual_needs_money_in_strong_only() {
        let cfg = |extra: &str| -> Config {
            toml::from_str(&format!("rpi = '/r'\nhome = '/h'\n{extra}")).unwrap()
        };
        let manual = cfg("[cards]\nprovider = 'manual'\n");
        let e = manual.validate().unwrap_err().to_string();
        assert_eq!(e, "[cards] needs approvals.strong_only to include money");
        let strong = "[approvals]\nstrong_only = ['loosen', 'money']\n";
        assert!(cfg(&format!("[cards]\nprovider = 'manual'\n{strong}"))
            .validate()
            .is_ok());
        // Link asks the owner itself, so its approval is the strong step.
        assert!(cfg("[cards]\nprovider = 'link'\n").validate().is_ok());
        for bad in [
            "provider = 'stripe'",
            "provider = 'fake'",
            "max = 0",
            "minutes = 0",
            "minutes = 61",
        ] {
            let c = cfg(&format!("[cards]\n{bad}\n"));
            assert!(c.validate().is_err(), "{bad}");
        }
        let mut fake = cfg("[cards]\nprovider = 'fake'\n");
        fake.cards_fake = true;
        assert!(fake.validate().is_ok(), "the tests' issuer");
        assert_eq!(crate::cards::max_minor(&cfg("")), 10_000);
        assert_eq!(crate::cards::minutes(&cfg("[cards]\nminutes = 5\n")), 5);
    }

    #[test]
    fn parses_and_checks_owners() {
        let cfg: Config = toml::from_str(
            r#"
            rpi = "/opt/rpi"
            home = "/home/zephyr/agent"
            model = "anthropic/claude-sonnet-5"
            [discord]
            token_env = "ZEPHYR_DISCORD_TOKEN"
            [imessage]
            listen = "100.64.0.1:7791"
            server_url = "http://100.64.0.2:1234"
            password_env = "ZEPHYR_BB_PASSWORD"
            secret_env = "ZEPHYR_BB_SECRET"
            [owners]
            chat = ["1000"]
            discord = ["2000"]
            "#,
        )
        .unwrap();
        assert!(cfg.is_owner("chat", "1000"));
        assert!(!cfg.is_owner("chat", "1001"));
        assert!(cfg.is_owner("discord", "2000"));
        assert!(!cfg.is_owner("discord", "1000"));
        assert_eq!(cfg.discord.unwrap().token_env, "ZEPHYR_DISCORD_TOKEN");
        assert_eq!(cfg.imessage.unwrap().lookback_hours, 24);
        assert!(toml::from_str::<Config>("rpi='a'\nhome='b'\ntypo=1").is_err());
    }

    /// DESIGN §4.24: no `[browser]` reads the default pace; the table is the owner's to add.
    #[test]
    fn no_browser_table_reads_the_default_pace() {
        let cfg: Config = toml::from_str("rpi = 'r'\nhome = 'h'\n").unwrap();
        assert_eq!((cfg.browser.clone(), cfg.pace_ms()), (None, 1500));
        let off: Config = toml::from_str("rpi='r'\nhome='h'\n[browser]\npace_ms = 0\n").unwrap();
        off.validate().unwrap();
        assert_eq!(off.pace_ms(), 0);
        let over: Config =
            toml::from_str("rpi='r'\nhome='h'\n[browser]\npace_ms = 60001\n").unwrap();
        let err = over.validate().unwrap_err().to_string();
        assert_eq!(err, "[browser] pace_ms must be 0 to 60000");
        assert!(toml::from_str::<Config>("rpi='r'\nhome='h'\n[browser]\npace = 1\n").is_err());
    }

    #[test]
    fn telegram_and_signal_tables_parse() {
        let cfg: Config = toml::from_str(
            "rpi = 'r'\nhome = 'h'\nstate = '/var/lib/zephyr'\n\
             [telegram]\ntoken_env = 'ZEPHYR_TELEGRAM_TOKEN'\n\
             [signal]\naccount = '+821012345678'\n\
             [owners]\ntelegram = ['55']\nsignal = ['7f3c']\n",
        )
        .unwrap();
        let tg = cfg.telegram.as_ref().unwrap();
        assert_eq!(tg.token_env, "ZEPHYR_TELEGRAM_TOKEN");
        assert!(tg.progress);
        assert_eq!(cfg.signal.as_ref().unwrap().account, "+821012345678");
        assert_eq!(cfg.telegram_api, "https://api.telegram.org");
        assert_eq!(
            cfg.signal_cli,
            Path::new("/opt/zephyr/signal/bin/signal-cli")
        );
        assert_eq!(
            crate::signal::data_dir(&cfg),
            Path::new("/var/lib/zephyr/signal")
        );
        assert!(cfg.is_owner("telegram", "55") && cfg.is_owner("signal", "7f3c"));
        assert!(toml::from_str::<Config>("rpi='a'\nhome='b'\n[signal]\nnumber='+1'").is_err());
    }

    #[test]
    fn a_wildcard_network_allow_is_refused() {
        let with = |allow: &str| -> Config {
            toml::from_str(&format!("rpi='a'\nhome='b'\n[network]\nallow = {allow:?}")).unwrap()
        };
        let e = with("*.gmail.com:993").validate().unwrap_err().to_string();
        assert!(e.starts_with("network.allow: "), "{e}");
        with("imap.gmail.com:993, github.com:22")
            .validate()
            .unwrap();
        assert!(toml::from_str::<Config>("rpi='a'\nhome='b'\n[network]\nallowed=''").is_err());
    }

    #[test]
    fn a_run_is_over_at_either_run_limit() {
        use std::time::Duration;
        let b: Budget = toml::from_str("run_turns = 3\nrun_minutes = 1.5").unwrap();
        assert_eq!(b.run_over(2, Duration::from_secs(89)), None);
        assert_eq!(
            b.run_over(3, Duration::ZERO).unwrap(),
            "3 turns (budget.run_turns)"
        );
        assert_eq!(
            b.run_over(0, Duration::from_secs(90)).unwrap(),
            "1.5 minutes (budget.run_minutes)"
        );
        assert_eq!(Budget::default().run_over(1000, Duration::MAX), None);
    }

    #[test]
    fn budget_is_over_when_either_limit_is_reached() {
        let b: Budget = toml::from_str("daily_tokens = 100\ndaily_cost_usd = 2.5").unwrap();
        assert!(!b.exceeded(99, 2.49));
        assert!(b.exceeded(100, 0.0));
        assert!(b.exceeded(0, 2.5));
        assert!(!Budget::default().exceeded(i64::MAX, f64::MAX));
        assert!(toml::from_str::<Budget>("daily_token = 1").is_err());
    }

    #[test]
    fn tasks_parallel_defaults_to_two_and_is_one_to_thirty_two() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert_eq!(cfg.parallel_tasks(), 2);
        cfg.validate().unwrap();
        let with = |text: &str| -> Config { toml::from_str(&format!("{base}{text}")).unwrap() };
        assert_eq!(with("[tasks]\n").parallel_tasks(), 2);
        assert_eq!(with("[tasks]\nparallel = 16\n").parallel_tasks(), 16);
        with("[tasks]\nparallel = 4\n").validate().unwrap();
        for bad in ["0", "33"] {
            let cfg = with(&format!("[tasks]\nparallel = {bad}\n"));
            assert!(cfg.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>(&format!("{base}[tasks]\ntypo = 1\n")).is_err());
    }

    #[test]
    fn subagents_and_the_task_model_are_checked() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert!(cfg.subagents.is_empty() && cfg.task_model().is_none());
        let with = |text: &str| -> Config { toml::from_str(&format!("{base}{text}")).unwrap() };
        let cfg = with(
            r#"[tasks]
model = "claude-code/claude-haiku-5-5"
[[subagent]]
name = "explorer"
model = "p/m"
prompt = """You explore.
No edits."""
[[subagent]]
name = "impl_2-x"
prompt = "Build it."
"#,
        );
        cfg.validate().unwrap();
        assert_eq!(cfg.parallel_tasks(), 2);
        assert_eq!(cfg.task_model(), Some("claude-code/claude-haiku-5-5"));
        assert_eq!(cfg.subagents[0].prompt, "You explore.\nNo edits.");
        assert_eq!(cfg.subagents[1].model, None);
        let sub = |name: &str, model: &str, prompt: &str| {
            format!("[[subagent]]\nname = {name:?}\n{model}prompt = {prompt:?}\n")
        };
        let long = "x".repeat(MAX_SUBAGENT_PROMPT + 1);
        for bad in [
            sub("Explorer", "", "p"),
            sub("", "", "p"),
            sub("-x", "", "p"),
            sub(&"x".repeat(33), "", "p"),
            sub("a", "", "p") + &sub("a", "", "q"),
            sub("a", "", " \n "),
            sub("a", "", &long),
            sub("a", "model = 'gpt-5'\n", "p"),
            sub("a", "model = 'p/m x'\n", "p"),
            "[tasks]\nmodel = 'gpt-5'\n".into(),
        ] {
            assert!(with(&bad).validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>(&format!(
            "{base}[[subagent]]\nname = 'a'\nprompt = 'p'\ntypo = 1\n"
        ))
        .is_err());
    }

    #[test]
    fn strong_channels_default_to_the_tui_chat_and_passkey_and_keep_the_tui() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert_eq!(cfg.settings.strong_channels, ["tui", "chat", "passkey"]);
        let with = |text: &str| -> Config { toml::from_str(&format!("{base}{text}")).unwrap() };
        with("[settings]\nstrong_channels = ['tui']\n")
            .validate()
            .unwrap();
        for bad in ["['chat']", "[]", "['tui', 'whatsapp']"] {
            let cfg = with(&format!("[settings]\nstrong_channels = {bad}\n"));
            assert!(cfg.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>(&format!("{base}[settings]\ntypo = 1\n")).is_err());
    }

    #[test]
    fn strong_channels_may_name_web_and_passkey() {
        let cfg: Config = toml::from_str(
            "rpi = 'a'\nhome = 'b'\n[settings]\nstrong_channels = ['tui', 'web', 'passkey']\n",
        )
        .unwrap();
        cfg.validate().unwrap();
    }

    #[test]
    fn a_web_section_needs_a_bare_host() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let with = |text: &str| -> Config { toml::from_str(&format!("{base}{text}")).unwrap() };
        let cfg = with("[web]\nhost = 'box.tail1234.ts.net'\n");
        cfg.validate().unwrap();
        let web = cfg.web.unwrap();
        assert_eq!((web.port, web.tailnet_users.len()), (7793, 0));
        let cfg = with("[web]\nhost = 'h'\nport = 8000\ntailnet_users = ['owner@github']\n");
        cfg.validate().unwrap();
        assert_eq!(cfg.web.unwrap().port, 8000);
        for bad in [
            "https://x",
            "x:443",
            "x/",
            "x.",
            "",
            "X.ts.net",
            "x y",
            "x_y",
            "exämple.net",
        ] {
            let cfg = with(&format!("[web]\nhost = {bad:?}\n"));
            assert!(cfg.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>(&format!("{base}[web]\nhost = 'h'\ntypo = 1\n")).is_err());
        assert!(
            toml::from_str::<Config>(&format!("{base}[web]\nport = 1\n")).is_err(),
            "no host"
        );
        // The fields no file sets.
        let cfg: Config = toml::from_str(base).unwrap();
        assert!(cfg.web.is_none() && cfg.push_hosts.is_empty());
        assert_eq!(cfg.web_lockout, std::time::Duration::from_secs(15 * 60));
        assert_eq!(cfg.web_ping, std::time::Duration::from_secs(30));
    }

    #[test]
    fn auth_table_defaults_and_overrides() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert!(cfg.auth.enabled);
        assert_eq!(cfg.auth.refresh_minutes, 10.0);
        let cfg: Config = toml::from_str(&format!(
            "{base}[auth]\nrefresh_minutes = 3\nenabled = false\n"
        ))
        .unwrap();
        assert!(!cfg.auth.enabled);
        assert_eq!(cfg.auth.refresh_minutes, 3.0);
        assert!(toml::from_str::<Config>(&format!("{base}[auth]\ntypo = 1\n")).is_err());
    }

    #[test]
    fn accounts_parse_in_order_and_names_are_checked() {
        let base = "rpi = 'a'\nhome = '/z'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert!(cfg.accounts.is_empty());
        let pool = |names: &[&str]| -> Config {
            let mut text = base.to_string();
            for n in names {
                text += &format!("[[account]]\nname = {n:?}\nmodel = 'openai-codex/gpt-5.5'\n");
            }
            toml::from_str(&text).unwrap()
        };
        let cfg = pool(&["codex-main", "claude_max.2"]);
        cfg.validate().unwrap();
        assert_eq!(cfg.accounts[1].name, "claude_max.2");
        assert_eq!(cfg.accounts[0].provider(), "openai-codex");
        assert_eq!(
            cfg.account_dir(&cfg.accounts[0]),
            PathBuf::from("/z/accounts/codex-main")
        );
        for bad in [
            &["a", "a"][..],
            &[""],
            &[".."],
            &[".hidden"],
            &["a/b"],
            &["a b"],
        ] {
            assert!(pool(bad).validate().is_err(), "{bad:?}");
        }
        let mut cfg = pool(&["a"]);
        cfg.accounts[0].model = "no-provider".into();
        assert!(cfg.validate().is_err());
        let mut cfg = pool(&["a"]);
        cfg.agent.env = vec!["OK_1".into()];
        cfg.validate().unwrap();
        cfg.agent.env = vec!["NOT-A-NAME".into()];
        assert!(cfg.validate().is_err());
        assert!(toml::from_str::<Config>(&format!(
            "{base}[[account]]\nname = 'a'\nmodel = 'p/m'\ntypo = 1\n"
        ))
        .is_err());
    }

    #[test]
    fn extensions_default_empty_and_parse_in_order() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert!(cfg.extensions.is_empty());
        let cfg: Config = toml::from_str(&format!(
            "{base}extensions = ['/opt/zephyr/ext/librpi_providers.so', '/opt/zephyr/ext/libzephyr_cursor.so']\n"
        ))
        .unwrap();
        assert_eq!(
            cfg.extensions,
            [
                PathBuf::from("/opt/zephyr/ext/librpi_providers.so"),
                PathBuf::from("/opt/zephyr/ext/libzephyr_cursor.so"),
            ]
        );
    }

    #[test]
    fn a_review_time_must_be_hh_mm() {
        let load = |at: &str| {
            let cfg: Config = toml::from_str(&format!(
                "rpi = \"/r\"\nhome = \"/h\"\n[review]\nat = \"{at}\"\n"
            ))
            .unwrap();
            cfg.validate().map_err(|e| e.to_string())
        };
        assert!(load("21:30").is_ok());
        assert!(load("00:00").is_ok() && load("23:59").is_ok());
        for bad in ["25:00", "9:30", "21:5", "21:60", "2130", "ab:cd"] {
            assert_eq!(
                load(bad).unwrap_err(),
                "[review] at must be HH:MM, e.g. 21:30",
                "{bad}"
            );
        }
    }

    #[test]
    fn digest_needs_a_review_time() {
        let load = |extra: &str| {
            let cfg: Config =
                toml::from_str(&format!("rpi = \"/r\"\nhome = \"/h\"\n{extra}")).unwrap();
            cfg.validate()
                .map(|()| cfg.digest())
                .map_err(|e| e.to_string())
        };
        assert_eq!(load(""), Ok(false));
        assert_eq!(load("[memory]\nnotify = \"each\"\n"), Ok(false));
        assert_eq!(
            load("[memory]\nnotify = \"digest\"\n[review]\nat = \"21:30\"\n"),
            Ok(true)
        );
        assert_eq!(
            load("[memory]\nnotify = \"digest\"\n"),
            Err("[memory] notify = \"digest\" needs [review] at: the digest goes in the daily review".into())
        );
        assert_eq!(
            load("[memory]\nnotify = \"weekly\"\n"),
            Err("[memory] notify must be each or digest".into())
        );
    }

    #[test]
    fn memory_auto_is_off_unless_set_to_owner() {
        let load = |extra: &str| {
            let cfg: Config =
                toml::from_str(&format!("rpi = \"/r\"\nhome = \"/h\"\n{extra}")).unwrap();
            cfg.validate()
                .map(|()| cfg.memory_auto().to_string())
                .map_err(|e| e.to_string())
        };
        assert_eq!(load(""), Ok("off".into()), "an install with no key");
        assert_eq!(load("[memory]\nnotify = \"each\"\n"), Ok("off".into()));
        assert_eq!(load("[memory]\nauto = \"owner\"\n"), Ok("owner".into()));
        assert_eq!(
            load("[memory]\nauto = \"always\"\n"),
            Err("[memory] auto must be off or owner".into())
        );
    }
}
