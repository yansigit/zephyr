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
    /// `--model` for the child, e.g. `anthropic/claude-sonnet-5`.
    pub model: Option<String>,
    /// The `zephyr-policy` cdylib, loaded into every child with `--extension`. When set, a
    /// child that does not list the `zephyr-policy` command (rpi skips a plugin that fails
    /// to load) is refused and gets no prompts.
    pub policy_plugin: Option<PathBuf>,
    /// The Discord adapter. Its owner list is `owners.discord`. Absent means no Discord.
    pub discord: Option<crate::discord::Config>,
    /// The BlueBubbles iMessage adapter. Its owner list is `owners.imessage` (handles). Absent means no iMessage.
    pub imessage: Option<crate::imessage::Config>,
    /// Owner allowlist per channel name. Senders not listed are dropped.
    #[serde(default)]
    pub owners: HashMap<String, Vec<String>>,
    /// The `rpi --connect` attach listener (DESIGN §4.3). Off when absent.
    pub attach: Option<Attach>,
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
        Ok(toml::from_str(&text)?)
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
}
