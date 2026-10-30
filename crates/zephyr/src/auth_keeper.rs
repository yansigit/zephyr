//! AuthKeeper (DESIGN §4.7), until upstream PR #144 lands. Refreshes OAuth credentials that
//! expire soon, then restarts the child so the new token binds.
//!
//! Refresh goes through `rpi auth refresh --provider <id>` as a subprocess, not the `oauth`
//! RPC. Upstream `rpi-cli/src/auth.rs` `run_refresh` reads the stored credential and passes it
//! to the plugin as `{"credential": {access, refresh, expires_at}}`. The RPC handler
//! (`modes.rs` `rpc_oauth`) forwards only the request's `input`, default `{}`, so it would not
//! give the plugin the refresh token.
//!
//! Why a restart and not `set_model`: upstream builds each provider's request auth once, when
//! the session starts (`rpi-runtime/src/provider/binding.rs`, `RequestAuth`). `set_model` only
//! swaps the model descriptor (`rpi-harness/src/agent_harness.rs`, `set_model`), so it keeps the
//! old token. Tokens are never logged here.

use crate::config::Config;
use crate::rpc::Child;
use anyhow::{bail, Context};
use serde::Deserialize;
use serde_json::json;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// A credential expiring within this many seconds gets refreshed.
const EXPIRY_WINDOW_SECS: i64 = 15 * 60;

/// `[auth]`. Absent means the defaults: enabled, checked every 10 minutes.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct AuthConfig {
    pub enabled: bool,
    /// Minutes between checks. Fractions are allowed; the router clamps to at least a second.
    pub refresh_minutes: f64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            refresh_minutes: 10.0,
        }
    }
}

/// The rpi agent dir, resolved as rpi resolves it (`RPI_CODING_AGENT_DIR`, else `~/.rpi/agent`).
pub fn agent_dir(cfg: &Config) -> PathBuf {
    cfg.agent_dir
        .clone()
        .or_else(|| std::env::var_os("RPI_CODING_AGENT_DIR").map(PathBuf::from))
        .unwrap_or_else(|| {
            PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".rpi/agent")
        })
}

/// Only the type and expiry of each `auth.json` entry are read. Tokens are never deserialized.
#[derive(Deserialize)]
struct StoredExpiry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    expires: i64,
}

/// Expiry (Unix seconds, 0 = unknown) of each stored OAuth credential. Empty if there is no file.
/// `get_auth_status` has no expiry field, so the file is the only source.
fn oauth_expiries(agent_dir: &Path) -> anyhow::Result<HashMap<String, i64>> {
    let _fs = crate::agent::fs();
    let path = agent_dir.join("auth.json");
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let store: HashMap<String, StoredExpiry> =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    Ok(store
        .into_iter()
        .filter(|(_, c)| c.kind == "oauth")
        .map(|(provider, c)| (provider, c.expires))
        .collect())
}

/// One check. Asks the child which providers hold OAuth credentials, then runs
/// `rpi auth refresh` for each one that expires within the window and that a loaded extension can
/// refresh. Returns true when at least one credential was refreshed, so the caller restarts the child.
pub async fn refresh(child: &mut Child, cfg: &Config, now: i64) -> anyhow::Result<bool> {
    let agent_dir = agent_dir(cfg);
    let status = child.call(json!({"type": "get_auth_status"})).await?;
    let providers = status["providers"].as_array().cloned().unwrap_or_default();
    let oauth: Vec<&serde_json::Value> = providers.iter().filter(|p| p["oauth"] == true).collect();
    if oauth.is_empty() {
        return Ok(false);
    }
    let expiries = oauth_expiries(&agent_dir)?;
    let mut refreshed = false;
    for entry in oauth {
        let provider = entry["provider"].as_str().unwrap_or_default();
        let expires = expiries.get(provider).copied().unwrap_or(0);
        // 0 means the expiry is unknown, so there is nothing to act on.
        if expires == 0 || expires - now > EXPIRY_WINDOW_SECS {
            continue;
        }
        if entry["oauthAvailable"] != true {
            eprintln!("zephyr: {provider} OAuth credential expires soon, but no loaded extension can refresh it");
            continue;
        }
        match run_refresh(cfg, &agent_dir, provider).await {
            Ok(()) => {
                eprintln!("zephyr: refreshed the {provider} OAuth credential");
                refreshed = true;
            }
            Err(e) => eprintln!("zephyr: {provider} OAuth refresh failed: {e:#}"),
        }
    }
    Ok(refreshed)
}

/// Pool mode: each account's own dir, for its model's provider. Needs no child, so accounts
/// that are cooling down stay fresh. Returns true when `current`'s credential was refreshed.
pub async fn refresh_accounts(
    cfg: &Config,
    current: Option<&crate::config::Account>,
    now: i64,
) -> bool {
    let mut restart = false;
    for account in &cfg.accounts {
        let dir = match std::path::absolute(cfg.account_dir(account)) {
            Ok(dir) => dir,
            Err(e) => {
                eprintln!("zephyr: account {}: {e}", account.name);
                continue;
            }
        };
        let provider = account.provider();
        let expires = match oauth_expiries(&dir) {
            Ok(e) => e.get(provider).copied().unwrap_or(0),
            Err(e) => {
                eprintln!("zephyr: account {}: {e:#}", account.name);
                continue;
            }
        };
        if expires == 0 || expires - now > EXPIRY_WINDOW_SECS {
            continue;
        }
        match run_refresh(cfg, &dir, provider).await {
            Ok(()) => {
                eprintln!(
                    "zephyr: refreshed the {provider} OAuth credential of account {}",
                    account.name
                );
                restart |= current.is_some_and(|c| c.name == account.name);
            }
            Err(e) => eprintln!(
                "zephyr: account {} {provider} OAuth refresh failed: {e:#}",
                account.name
            ),
        }
    }
    restart
}

/// `rpi auth refresh --provider <id>`, with the child's agent dir and cwd. Its output is not
/// logged, since it can echo credential details.
async fn run_refresh(cfg: &Config, agent_dir: &Path, provider: &str) -> anyhow::Result<()> {
    let mut cmd = tokio::process::Command::new(&cfg.rpi);
    crate::agent::cmd(cmd.as_std_mut());
    let out = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        cmd.args(["auth", "refresh", "--provider", provider])
            .env("RPI_CODING_AGENT_DIR", agent_dir)
            .current_dir(&cfg.home)
            .output(),
    )
    .await
    .context("timed out")??;
    if !out.status.success() {
        bail!("rpi auth refresh exited with {}", out.status);
    }
    Ok(())
}

pub fn now_secs() -> i64 {
    (crate::unix_millis() / 1000) as i64
}
