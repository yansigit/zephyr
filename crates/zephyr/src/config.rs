//! `zephyr.toml`: where rpi lives, where state lives, and who may talk to the agent.

use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// The pinned `rpi` binary.
    pub rpi: PathBuf,
    /// State dir: `state.db`, `sessions/`, `zephyr.sock`. Also the child's cwd.
    pub home: PathBuf,
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
    /// IANA time zone for job cron schedules, e.g. `Asia/Seoul`. Default: the system's.
    pub timezone: Option<String>,
    /// Wall clock cap for one job run. Not configurable; tests shorten it.
    #[serde(skip, default = "job_timeout")]
    pub job_timeout: std::time::Duration,
    /// OAuth refresh (DESIGN §4.7). Defaults apply when the table is absent.
    #[serde(default)]
    pub auth: crate::auth_keeper::AuthConfig,
}

fn job_timeout() -> std::time::Duration {
    crate::scheduler::JOB_TIMEOUT
}

/// Sent once, when a budget limit is crossed.
pub const BUDGET_NOTICE: &str =
    "[zephyr] daily budget reached: paused until the next UTC day or /resume";

/// `[budget]`: counted per UTC day. Crossing a limit pauses the gateway until the next UTC day or `/resume`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Budget {
    pub daily_tokens: Option<i64>,
    pub daily_cost_usd: Option<f64>,
}

impl Budget {
    pub fn exceeded(&self, tokens: i64, cost: f64) -> bool {
        self.daily_tokens.is_some_and(|t| tokens >= t)
            || self.daily_cost_usd.is_some_and(|c| cost >= c)
    }
}

/// One `[[account]]`. Its credentials live in `<home>/accounts/<name>/`.
#[derive(Debug, Clone, Deserialize)]
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

/// `[attach]`: listens on 127.0.0.1 only. The token comes from exactly one of the two sources.
#[derive(Debug, Clone, Deserialize)]
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
        let cfg: Self = toml::from_str(&text)?;
        cfg.validate()?;
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
        Ok(())
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

    pub fn db_path(&self) -> PathBuf {
        self.home.join("state.db")
    }

    pub fn session_dir(&self) -> PathBuf {
        self.home.join("sessions")
    }

    pub fn socket_path(&self) -> PathBuf {
        self.home.join("zephyr.sock")
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
    fn budget_is_over_when_either_limit_is_reached() {
        let b: Budget = toml::from_str("daily_tokens = 100\ndaily_cost_usd = 2.5").unwrap();
        assert!(!b.exceeded(99, 2.49));
        assert!(b.exceeded(100, 0.0));
        assert!(b.exceeded(0, 2.5));
        assert!(!Budget::default().exceeded(i64::MAX, f64::MAX));
        assert!(toml::from_str::<Budget>("daily_token = 1").is_err());
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
            "{base}extensions = ['/opt/zephyr/ext/librpi_providers.so', '/opt/zephyr/ext/rpi-cursor-lite/librpi_cursor_lite.so']\n"
        ))
        .unwrap();
        assert_eq!(
            cfg.extensions,
            [
                PathBuf::from("/opt/zephyr/ext/librpi_providers.so"),
                PathBuf::from("/opt/zephyr/ext/rpi-cursor-lite/librpi_cursor_lite.so"),
            ]
        );
    }
}
