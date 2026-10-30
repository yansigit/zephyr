//! The account pool (DESIGN §4.9): which `[[account]]` the child runs on, when one is
//! rate-limited, and the per-account dirs rpi reads its credentials from.

use crate::config::{Account, Config};
use crate::store::{Cooldown, Store};
use serde_json::Value;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Cooldowns when the error names no reset time: 15 min, 1 h, then 4 h.
const BACKOFF: [i64; 3] = [15 * 60, 60 * 60, 4 * 60 * 60];

/// Whether a failed turn's error is a rate limit. `Some(resets_at)` when it is; `resets_at`
/// comes from the providers' `rate_limited; resets_at=<unix>` marker, else a text match.
pub fn rate_limit(error: &str) -> Option<Option<i64>> {
    let lower = error.to_ascii_lowercase();
    if lower.contains("rate_limited;") {
        let resets = lower
            .split("resets_at=")
            .nth(1)
            .and_then(|r| r.split(|c: char| !c.is_ascii_digit()).next()?.parse().ok());
        return Some(resets);
    }
    // `429|rate.?limit|usage limit|quota|too many requests`, case-insensitive.
    let rate_limit = lower.match_indices("rate").any(|(i, _)| {
        let rest = &lower[i + 4..];
        rest.starts_with("limit")
            || rest
                .chars()
                .next()
                .is_some_and(|c| rest[c.len_utf8()..].starts_with("limit"))
    });
    let hit = rate_limit
        || ["429", "usage limit", "quota", "too many requests"]
            .iter()
            .any(|p| lower.contains(p));
    hit.then_some(None)
}

/// Marks `account` cooling down and returns when it ends. A reset time in the past counts
/// as none, so a failed account never comes straight back within the same turn.
pub fn cool_down(
    store: &Store,
    account: &str,
    error: &str,
    resets_at: Option<i64>,
    now: i64,
) -> rusqlite::Result<i64> {
    let level = store.cooldowns()?.get(account).map_or(0, |c| c.level);
    let reason: String = error.chars().take(200).collect();
    let c = match resets_at.filter(|&t| t > now) {
        Some(until) => Cooldown {
            until,
            reason,
            level,
        },
        None => Cooldown {
            until: now + BACKOFF[(level as usize).min(BACKOFF.len() - 1)],
            reason,
            level: level + 1,
        },
    };
    store.set_cooldown(account, &c)?;
    Ok(c.until)
}

/// The first account not cooling down, or the earliest reset when every one is.
pub fn pick<'a>(
    accounts: &'a [Account],
    cooling: &HashMap<String, Cooldown>,
    now: i64,
) -> Result<&'a Account, i64> {
    let until = |a: &Account| cooling.get(&a.name).map_or(0, |c| c.until);
    accounts
        .iter()
        .find(|a| until(a) <= now)
        .ok_or_else(|| accounts.iter().map(until).min().unwrap_or(now))
}

/// Creates the account's dir (owner-only: it holds `auth.json`) with links to the shared
/// `models.json` and `settings.json`, and returns it as an absolute path, as rpi requires.
pub fn prepare(cfg: &Config, account: &Account) -> anyhow::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let _fs = crate::agent::fs();
    let shared = crate::auth_keeper::agent_dir(cfg);
    let dir = std::path::absolute(cfg.account_dir(account))?;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    limit_retries(&shared.join("settings.json"))?;
    for f in ["models.json", "settings.json"] {
        let (src, link) = (shared.join(f), dir.join(f));
        if src.exists() && link.symlink_metadata().is_err() {
            std::os::unix::fs::symlink(&src, &link)?;
        }
    }
    Ok(dir)
}

/// rpi 0.12.0 retries an error whose text says `429`, `rate limit` or `too many requests` up to
/// 10 times with backoff (`retry.maxRetries`, `rpi-harness` `is_retryable_assistant_error`).
/// The pool caps that at one retry, so failover starts within seconds.
fn limit_retries(path: &Path) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => "{}".into(),
        Err(e) => return Err(e.into()),
    };
    let mut v = match serde_json::from_str::<Value>(&text) {
        Ok(v) if v.is_object() && (v["retry"].is_null() || v["retry"].is_object()) => v,
        _ => {
            eprintln!(
                "zephyr: {} is not plain JSON; rpi's retries are left as they are",
                path.display()
            );
            return Ok(());
        }
    };
    if v["retry"]["maxRetries"].as_u64().is_some_and(|n| n <= 1) {
        return Ok(());
    }
    v["retry"]["maxRetries"] = 1.into();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.zephyr-tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(&v)?)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Whether the account's `auth.json` has an entry for its provider. Only keys are read.
fn logged_in(dir: &Path, provider: &str) -> bool {
    let _fs = crate::agent::fs();
    std::fs::read_to_string(dir.join("auth.json"))
        .ok()
        .and_then(|t| serde_json::from_str::<HashMap<String, serde::de::IgnoredAny>>(&t).ok())
        .is_some_and(|m| m.contains_key(provider))
}

/// `2026-10-28T12:00:00Z`.
pub fn iso(secs: i64) -> String {
    jiff::Timestamp::from_second(secs).map_or(secs.to_string(), |t| t.to_string())
}

/// One line per account for `zephyr status`: ready, cooling, or needs login, and today's usage.
pub fn status(cfg: &Config, store: &Store, now: i64) -> anyhow::Result<Vec<String>> {
    let cooling = store.cooldowns()?;
    let day = store.today()?;
    cfg.accounts
        .iter()
        .map(|a| {
            let state = match cooling.get(&a.name) {
                Some(c) if c.until > now => {
                    format!("cooling until {} ({})", iso(c.until), c.reason)
                }
                _ if !logged_in(&cfg.account_dir(a), a.provider()) => "needs login".into(),
                _ => "ready".into(),
            };
            let (tokens, cost) = store.account_spend(&day, &a.name)?;
            Ok(format!(
                "account {} ({}): {state}; today {tokens} tokens, ${cost:.4}",
                a.name, a.model
            ))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_carries_the_reset_and_the_text_fallback_does_not() {
        assert_eq!(
            rate_limit("rate_limited; resets_at=1793250433"),
            Some(Some(1793250433))
        );
        assert_eq!(
            rate_limit("codex: rate_limited; resets_at=17x"),
            Some(Some(17))
        );
        assert_eq!(rate_limit("rate_limited; weekly cap"), Some(None));
        for text in [
            "http error 429: slow down",
            "Rate limit reached",
            "rate-limit exceeded",
            "RATELIMIT",
            "You have hit your usage limit",
            "insufficient_quota",
            "Too Many Requests",
        ] {
            assert_eq!(rate_limit(text), Some(None), "{text}");
        }
        for text in [
            "invalid request",
            "rate of change",
            "limit reached",
            "500 server error",
        ] {
            assert_eq!(rate_limit(text), None, "{text}");
        }
    }

    #[test]
    fn backoff_climbs_and_a_reset_time_wins() {
        let dir = crate::test_dir("pool-backoff");
        let store = Store::open(&dir.join("state.db")).unwrap();
        let mut waits = Vec::new();
        for _ in 0..4 {
            waits.push(cool_down(&store, "a", "429", None, 1000).unwrap() - 1000);
        }
        assert_eq!(waits, [15 * 60, 60 * 60, 4 * 60 * 60, 4 * 60 * 60]);
        assert_eq!(cool_down(&store, "a", "x", Some(5000), 1000).unwrap(), 5000);
        assert_eq!(
            store.cooldowns().unwrap()["a"].level,
            4,
            "a reset time keeps the level"
        );
        assert_eq!(
            cool_down(&store, "b", "x", Some(999), 1000).unwrap(),
            1000 + 15 * 60
        );
        store.clear_cooldown("a").unwrap();
        assert_eq!(cool_down(&store, "a", "x", None, 0).unwrap(), 15 * 60);
    }

    #[test]
    fn picks_the_first_ready_account_or_the_earliest_reset() {
        let accounts: Vec<Account> = ["a", "b"]
            .map(|n| Account {
                name: n.into(),
                model: "p/m".into(),
            })
            .into();
        let cool = |pairs: &[(&str, i64)]| -> HashMap<String, Cooldown> {
            pairs
                .iter()
                .map(|(n, until)| {
                    let c = Cooldown {
                        until: *until,
                        reason: String::new(),
                        level: 0,
                    };
                    (n.to_string(), c)
                })
                .collect()
        };
        assert_eq!(pick(&accounts, &cool(&[]), 10).unwrap().name, "a");
        assert_eq!(pick(&accounts, &cool(&[("a", 20)]), 10).unwrap().name, "b");
        assert_eq!(pick(&accounts, &cool(&[("a", 10)]), 10).unwrap().name, "a");
        assert_eq!(
            pick(&accounts, &cool(&[("a", 30), ("b", 20)]), 10).unwrap_err(),
            20
        );
    }

    #[test]
    fn status_shows_ready_cooling_and_needs_login_without_reading_secrets() {
        let home = crate::test_dir("pool-status");
        let cfg: Config = toml::from_str(&format!(
            "rpi = 'r'\nhome = {home:?}\n[[account]]\nname = 'a'\nmodel = 'anthropic/m'\n[[account]]\nname = 'b'\nmodel = 'openai-codex/m'\n[[account]]\nname = 'c'\nmodel = 'cursor/m'\n"
        ))
        .unwrap();
        for (name, provider) in [("a", "anthropic"), ("b", "openai-codex")] {
            let dir = cfg.account_dir(
                &cfg.accounts
                    .iter()
                    .find(|a| a.name == name)
                    .unwrap()
                    .clone(),
            );
            std::fs::create_dir_all(&dir).unwrap();
            let auth = format!(r#"{{"{provider}": {{"type": "oauth", "access": "SECRET"}}}}"#);
            std::fs::write(dir.join("auth.json"), auth).unwrap();
        }
        let store = Store::open(&cfg.db_path()).unwrap();
        cool_down(&store, "b", "rate_limited", Some(2_000_000_000), 0).unwrap();
        store.add_account_spend("a", (12, 0.5)).unwrap();
        let lines = status(&cfg, &store, 1000).unwrap();
        assert_eq!(
            lines,
            [
                "account a (anthropic/m): ready; today 12 tokens, $0.5000",
                "account b (openai-codex/m): cooling until 2033-05-18T03:33:20Z (rate_limited); today 0 tokens, $0.0000",
                "account c (cursor/m): needs login; today 0 tokens, $0.0000",
            ]
        );
    }

    #[test]
    fn prepare_links_the_shared_files_and_caps_retries() {
        let root = crate::test_dir("pool-prepare");
        let agent = root.join("agent");
        std::fs::create_dir_all(&agent).unwrap();
        std::fs::write(agent.join("models.json"), "{}").unwrap();
        let cfg: Config = toml::from_str(&format!(
            "rpi = 'r'\nhome = {:?}\nagent_dir = {agent:?}\n[[account]]\nname = 'a'\nmodel = 'p/m'\n",
            root.join("home")
        ))
        .unwrap();
        let dir = prepare(&cfg, &cfg.accounts[0]).unwrap();
        assert_eq!(dir, root.join("home/accounts/a"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777,
            0o700
        );
        // No settings.json yet: one is written with the cap, then linked.
        let settings: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("settings.json")).unwrap())
                .unwrap();
        assert_eq!(settings, serde_json::json!({"retry": {"maxRetries": 1}}));
        assert_eq!(
            std::fs::read_link(dir.join("models.json")).unwrap(),
            agent.join("models.json")
        );
        // Idempotent, and a file with comments is left alone.
        prepare(&cfg, &cfg.accounts[0]).unwrap();
        std::fs::write(agent.join("settings.json"), "// mine\n{}").unwrap();
        prepare(&cfg, &cfg.accounts[0]).unwrap();
        assert_eq!(
            std::fs::read_to_string(agent.join("settings.json")).unwrap(),
            "// mine\n{}"
        );
    }
}
