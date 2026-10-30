//! Settings apply while zephyr runs: the router reloads `zephyr.toml` and `secrets.env` on
//! `/reload` or when either file's mtime changes, and applies each change the least
//! disruptive way (`diff`). `/model` and `/status` live here too.

use crate::config::{Account, Config};
use crate::host::conf;
use crate::store::Store;
use std::path::Path;
use std::time::SystemTime;
use toml_edit::DocumentMut;

/// What a config change needs, least disruptive first.
#[derive(Debug, PartialEq)]
pub enum Plan {
    Same,
    /// Read where it is used: budget, owners, timezone, auth, the pool order.
    Live,
    /// The running child switches with rpi's `set_model` (same provider, so the same sign-in).
    SetModel(String),
    /// A fresh child on the same session, once the current reply is done.
    RestartChild,
    /// The daemon execs itself once idle: channels, the attach listener, paths, secrets.
    Reexec,
}

/// `active` is the account the child runs on; `next` is the one the pool would pick from `new`.
pub fn diff(old: &Config, new: &Config, active: Option<&Account>, next: Option<&Account>) -> Plan {
    if old.rpi != new.rpi
        || old.home != new.home
        || old.discord != new.discord
        || old.imessage != new.imessage
        || old.attach != new.attach
        || old.agent != new.agent
    {
        return Plan::Reexec;
    }
    if old.extensions != new.extensions
        || old.policy_plugin != new.policy_plugin
        || old.agent_dir != new.agent_dir
    {
        return Plan::RestartChild;
    }
    let model = match (active, next) {
        (None, None) => (old.model != new.model).then(|| (old.model.clone(), new.model.clone())),
        (Some(a), Some(n)) if a.name == n.name => {
            (a.model != n.model).then(|| (Some(a.model.clone()), Some(n.model.clone())))
        }
        _ => return Plan::RestartChild,
    };
    match model {
        Some((Some(from), Some(to))) if provider(&from) == provider(&to) => Plan::SetModel(to),
        Some(_) => Plan::RestartChild,
        None if old == new => Plan::Same,
        None => Plan::Live,
    }
}

fn provider(model: &str) -> &str {
    model.split('/').next().unwrap_or_default()
}

/// A file's mtime, the reload trigger for hand edits. `None` when it is missing.
pub fn stamp(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

/// Points the config at `model`: the active account when it is on that provider, else the
/// first account that is, moved to the front of the pool so it becomes the active one.
pub fn set_model(doc: &mut DocumentMut, active: Option<&str>, model: &str) -> anyhow::Result<()> {
    let p = match model.split_once('/') {
        Some((p, id)) if !p.is_empty() && !id.is_empty() => p,
        _ => anyhow::bail!("usage: /model PROVIDER/MODEL"),
    };
    let accounts = conf::accounts(doc);
    if accounts.is_empty() {
        return conf::set(doc, "model", model);
    }
    let on = |a: &&(String, String)| provider(&a.1) == p;
    let name = accounts
        .iter()
        .filter(on)
        .find(|a| Some(a.0.as_str()) == active)
        .or_else(|| accounts.iter().find(on))
        .map(|a| a.0.clone())
        .ok_or_else(|| anyhow::anyhow!("no account is signed in to {p}; run `zephyr login {p}`"))?;
    conf::upsert_account(doc, &name, model);
    if accounts.len() == 1 {
        conf::set(doc, "model", model)?;
    }
    if accounts[0].0 != name {
        let mut order: Vec<String> = accounts.into_iter().map(|a| a.0).collect();
        order.retain(|n| *n != name);
        order.insert(0, name);
        conf::order_accounts(doc, &order)?;
    }
    Ok(())
}

/// What `zephyr status` and `/status` print, after the daemon line.
pub fn status_lines(cfg: &Config, store: &Store) -> anyhow::Result<Vec<String>> {
    let paused = store.pause_reason()?;
    let day = store.today()?;
    let (tokens, cost) = store.spend(&day)?;
    let budget = cfg.budget.clone().unwrap_or_default();
    let limit = |v: Option<String>| v.unwrap_or_else(|| "none".into());
    let mut lines = vec![
        format!(
            "paused: {}",
            paused.map_or("no".into(), |r| format!("yes ({r})"))
        ),
        format!(
            "spend:  {day} UTC: {tokens} tokens (limit {}), ${cost:.4} (limit {})",
            limit(budget.daily_tokens.map(|v| v.to_string())),
            limit(budget.daily_cost_usd.map(|v| v.to_string()))
        ),
    ];
    lines.extend(crate::pool::status(cfg, store, crate::now())?);
    Ok(lines)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(extra: &str) -> Config {
        toml::from_str(&format!("rpi = '/r'\nhome = '/h'\n{extra}")).unwrap()
    }

    const POOL: &str =
        "[[account]]\nname = 'a'\nmodel = 'x/one'\n[[account]]\nname = 'b'\nmodel = 'y/two'\n";

    fn first(c: &Config) -> Option<&Account> {
        c.accounts.first()
    }

    #[test]
    fn each_change_gets_the_least_disruptive_plan() {
        let old = cfg(POOL);
        let same = |new: &Config| diff(&old, new, first(&old), first(new));
        assert_eq!(same(&cfg(POOL)), Plan::Same);
        assert_eq!(
            same(&cfg(&format!("{POOL}[budget]\ndaily_tokens = 5\n"))),
            Plan::Live
        );
        let on = |a: &str| POOL.replace("x/one", a);
        assert_eq!(same(&cfg(&on("x/three"))), Plan::SetModel("x/three".into()));
        assert_eq!(same(&cfg(&on("z/three"))), Plan::RestartChild);
        // b first: the active account changes.
        let swapped =
            "[[account]]\nname = 'b'\nmodel = 'y/two'\n[[account]]\nname = 'a'\nmodel = 'x/one'\n";
        assert_eq!(same(&cfg(swapped)), Plan::RestartChild);
        // b cooling down keeps a active: order alone is live.
        assert_eq!(
            diff(&old, &cfg(swapped), first(&old), old.accounts.first()),
            Plan::Live
        );
        assert_eq!(
            same(&cfg(&format!("extensions = ['/e.so']\n{POOL}"))),
            Plan::RestartChild
        );
        assert_eq!(
            same(&cfg(&format!("{POOL}[discord]\ntoken_env = 'T'\n"))),
            Plan::Reexec
        );
        assert_eq!(
            same(&cfg(&format!(
                "{POOL}[attach]\nport = 7\ntoken_env = 'T'\n"
            ))),
            Plan::Reexec
        );
        // What the agent's processes may see is fixed at start.
        assert_eq!(
            same(&cfg(&format!("{POOL}[agent]\nenv = ['K']\n"))),
            Plan::Reexec
        );
        // Without a pool, the top-level model.
        let solo = cfg("model = 'x/one'\n");
        let to = |m: &str| diff(&solo, &cfg(&format!("model = '{m}'\n")), None, None);
        assert_eq!(to("x/two"), Plan::SetModel("x/two".into()));
        assert_eq!(to("y/two"), Plan::RestartChild);
        assert_eq!(diff(&solo, &old, None, first(&old)), Plan::RestartChild);
    }

    #[test]
    fn set_model_picks_the_account_on_that_provider() {
        let mut doc: DocumentMut = format!("# keep\nrpi = '/r'\n{POOL}").parse().unwrap();
        set_model(&mut doc, Some("a"), "x/three").unwrap();
        assert_eq!(
            conf::accounts(&doc),
            [("a".into(), "x/three".into()), ("b".into(), "y/two".into())]
        );
        set_model(&mut doc, Some("a"), "y/four").unwrap();
        assert_eq!(
            conf::accounts(&doc),
            [
                ("b".into(), "y/four".into()),
                ("a".into(), "x/three".into())
            ]
        );
        assert!(doc.to_string().starts_with("# keep"));
        let err = set_model(&mut doc, Some("b"), "z/five").unwrap_err();
        assert!(err.to_string().contains("zephyr login z"), "{err}");
        assert!(set_model(&mut doc, None, "nope").is_err());

        let mut solo: DocumentMut = "model = 'x/one'\n".parse().unwrap();
        set_model(&mut solo, None, "y/two").unwrap();
        assert_eq!(conf::get(&solo, "model").as_deref(), Some("y/two"));
        let mut one: DocumentMut = "[[account]]\nname = 'a'\nmodel = 'x/one'\n"
            .parse()
            .unwrap();
        set_model(&mut one, Some("a"), "x/two").unwrap();
        assert_eq!(conf::get(&one, "model").as_deref(), Some("x/two"));
    }
}
