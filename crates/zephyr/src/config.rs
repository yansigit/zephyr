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
    /// Owner allowlist per channel name. Senders not listed are dropped.
    #[serde(default)]
    pub owners: HashMap<String, Vec<String>>,
    /// The `rpi --connect` attach listener (DESIGN §4.3). Off when absent.
    pub attach: Option<Attach>,
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
    /// The file this was loaded from, which a reload reads again. `None` when built in memory.
    #[serde(skip)]
    pub path: Option<PathBuf>,
    /// The projects folder `/rollback` checkpoints (DESIGN §4.14): `$ZEPHYR_PROJECTS`, which
    /// `zephyr up` sets. `None` turns checkpoints off. Not configurable; tests set it.
    #[serde(skip, default = "projects")]
    pub projects: Option<PathBuf>,
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
    vec!["tui".into(), "chat".into()]
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

impl Config {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?;
        let mut cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
        cfg.path = Some(path.to_path_buf());
        Ok(cfg)
    }

    /// Account names become directory names, so each must be a plain, unique path segment.
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
        if !(1..=32).contains(&self.parallel_tasks()) {
            anyhow::bail!("[tasks] parallel must be 1 to 32");
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
        // The attach token already proved the sender is the owner.
        channel == crate::attach::CHANNEL
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
    fn strong_channels_default_to_the_tui_and_chat_and_keep_the_tui() {
        let base = "rpi = 'a'\nhome = 'b'\n";
        let cfg: Config = toml::from_str(base).unwrap();
        assert_eq!(cfg.settings.strong_channels, ["tui", "chat"]);
        let with = |text: &str| -> Config { toml::from_str(&format!("{base}{text}")).unwrap() };
        with("[settings]\nstrong_channels = ['tui']\n")
            .validate()
            .unwrap();
        for bad in ["['chat']", "[]", "['tui', 'telegram']"] {
            let cfg = with(&format!("[settings]\nstrong_channels = {bad}\n"));
            assert!(cfg.validate().is_err(), "{bad}");
        }
        assert!(toml::from_str::<Config>(&format!("{base}[settings]\ntypo = 1\n")).is_err());
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
}
