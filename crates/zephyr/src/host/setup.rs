//! `init`, `login`, `accounts`, `model` and `channel`: the steps that ask questions.

use super::{conf, feed, home, output, run, Args, Host, USAGE};
use anyhow::{bail, Context};
use dialoguer::{Confirm, Input, MultiSelect, Password, Select, Sort};
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;

/// Provider ids as `rpi auth login --provider` takes them, with menu labels. They live with the
/// login registry (`crate::auth`), which the live gateway's `zephyr.login_providers` reports.
pub use crate::auth::PROVIDERS;

/// The Claude Code executable bundled with the Agent SDK in the image (rpi-claude-agent's stage.sh).
/// shortcut: the linux-x64 package only; an arm64 image needs `claude-agent-sdk-linux-arm64`.
pub const CLAUDE_CLI: &str =
    "/opt/zephyr/ext/rpi-claude-agent/runtime/node_modules/@anthropic-ai/claude-agent-sdk-linux-x64/claude";

fn provider_of(model: &str) -> &str {
    model.split('/').next().unwrap_or_default()
}

pub fn init(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let non_interactive = a.flag("--non-interactive") || !h.interactive;
    let no_start = a.flag("--no-start");
    let projects = a.value("--projects")?;
    let providers = a.values("--provider")?;
    let account_flags = a.values("--account")?;
    let model = a.value("--model")?;
    let rest = a.rest()?;
    if !rest.is_empty() {
        bail!("{USAGE}");
    }

    if !no_start && !super::quiet(super::docker(&["version"])) {
        bail!("docker is not reachable: install Docker and make sure `docker version` works");
    }
    h.data.ensure_layout()?;
    let current = h
        .data
        .projects()?
        .unwrap_or_else(|| home().join("Projects"));
    let projects = match projects {
        Some(p) => PathBuf::from(p),
        None if non_interactive => current,
        None => PathBuf::from(
            Input::<String>::new()
                .with_prompt("Projects folder the agent may work in")
                .default(current.display().to_string())
                .interact_text()?,
        ),
    };
    if !projects.is_dir() {
        bail!("{} is not a directory", projects.display());
    }
    h.data.set_projects(&projects.canonicalize()?)?;

    let mut doc = h.data.load()?;
    h.data.add_image_extensions(&mut doc)?;
    for spec in &account_flags {
        let (name, model) = spec
            .split_once('=')
            .filter(|(n, m)| !n.is_empty() && m.contains('/'))
            .with_context(|| format!("--account {spec}: want NAME=PROVIDER/MODEL"))?;
        known_provider(provider_of(model))?;
        conf::upsert_account(&mut doc, name, model);
    }
    for p in &providers {
        known_provider(p)?;
        if conf::accounts(&doc)
            .iter()
            .any(|(_, m)| provider_of(m) == p)
        {
            continue;
        }
        let m = model
            .as_deref()
            .filter(|m| provider_of(m) == p)
            .with_context(|| {
                format!(
                    "--provider {p}: also give --model {p}/<model> or --account NAME={p}/<model>"
                )
            })?;
        let name = next_name(&doc, p);
        conf::upsert_account(&mut doc, &name, m);
    }
    if let Some(m) = &model {
        conf::set(&mut doc, "model", m)?;
    }
    h.data.save(&doc)?;

    if non_interactive {
        println!("wrote {}", h.data.dir.display());
        for (name, m) in conf::accounts(&doc) {
            if !h.data.account(&name).join("auth.json").exists() {
                println!(
                    "  sign in later: zephyr login {} --account {name}",
                    provider_of(&m)
                );
            }
        }
        if !no_start {
            h.ensure_image(false, false)?;
            h.up()?;
            h.doctor(false)?;
        }
        return Ok(());
    }

    // Logins run in the container, so it starts before them.
    h.ensure_image(false, false)?;
    h.up()?;
    let have: Vec<String> = conf::accounts(&doc)
        .iter()
        .map(|(_, m)| provider_of(m).to_string())
        .collect();
    let defaults: Vec<bool> = PROVIDERS
        .iter()
        .map(|p| have.iter().any(|id| id == p.id))
        .collect();
    let labels: Vec<&str> = PROVIDERS.iter().map(|p| p.label).collect();
    let chosen = MultiSelect::new()
        .with_prompt("Providers (space to select, enter to confirm)")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;
    for i in chosen {
        let id = PROVIDERS[i].id;
        let mut add = !have.iter().any(|p| p == id);
        loop {
            if add {
                if let Err(e) = login(h, id, None, None, true) {
                    println!("{id}: {e:#}");
                }
            }
            add = Confirm::new()
                .with_prompt(format!("Add another {} account?", PROVIDERS[i].label))
                .default(false)
                .interact()?;
            if !add {
                break;
            }
        }
    }
    let mut doc = h.data.load()?;
    let names: Vec<String> = conf::accounts(&doc).into_iter().map(|(n, _)| n).collect();
    if names.len() > 1 {
        let order = Sort::new()
            .with_prompt("Failover order (first is used first)")
            .items(&names)
            .interact()?;
        let ordered: Vec<String> = order.into_iter().map(|i| names[i].clone()).collect();
        conf::order_accounts(&mut doc, &ordered)?;
        h.data.save(&doc)?;
    }
    for kind in ["discord", "imessage"] {
        let present = doc.get(kind).is_some();
        let prompt = if present {
            format!("Change the {kind} channel?")
        } else {
            format!("Set up {kind}?")
        };
        if Confirm::new()
            .with_prompt(prompt)
            .default(false)
            .interact()?
        {
            channel_add(h, kind)?;
        }
    }
    run(&mut super::docker(&["restart", &h.container]))?;
    h.doctor(false)
}

fn known_provider(id: &str) -> anyhow::Result<()> {
    if crate::auth::provider(id).is_none() {
        let ids: Vec<_> = PROVIDERS.iter().map(|p| p.id).collect();
        bail!("unknown provider {id}: one of {}", ids.join(", "));
    }
    Ok(())
}

/// `<provider>-<n>` with the smallest free n.
fn next_name(doc: &toml_edit::DocumentMut, provider: &str) -> String {
    crate::auth::next_name(doc, provider)
}

pub fn login_cmd(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let account = a.value("--account")?;
    let model = a.value("--model")?;
    let verify = !a.flag("--no-verify");
    match &a.rest()?[..] {
        // A new account joins the pool; a re-login gives the agent fresh credentials.
        [provider] => login(h, provider, account, model, verify).and_then(|()| h.apply()),
        _ => bail!("usage: zephyr login <provider> [--account NAME] [--model P/M] [--no-verify]"),
    }
}

/// The env var a key provider's login can be preset from; OAuth providers have none.
fn key_env(provider: &str) -> Option<&'static str> {
    match provider {
        "claude-code" => Some("CLAUDE_CODE_OAUTH_TOKEN"),
        "opencode-go" => Some("OPENCODE_API_KEY"),
        "cursor" => Some("CURSOR_API_KEY"),
        "anthropic" => Some("ANTHROPIC_API_KEY"),
        _ => None,
    }
}

/// Signs an account in through rpi in the container, records it in `[[account]]`, then tests it.
fn login(
    h: &Host,
    provider: &str,
    account: Option<String>,
    model: Option<String>,
    verify: bool,
) -> anyhow::Result<()> {
    known_provider(provider)?;
    h.need_running()?;
    let mut doc = h.data.load()?;
    let name = account.unwrap_or_else(|| next_name(&doc, provider));
    if name.is_empty() || name.contains(['/', '.']) {
        bail!("account name {name:?}: no '/' or '.'");
    }
    let dir = h.data.account(&name);
    std::fs::create_dir_all(&dir)?;
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let agent_dir = format!("/zephyr/accounts/{name}");
    let env = [("RPI_CODING_AGENT_DIR", agent_dir.as_str())];
    let exts = conf::extension_args(&doc);
    let exts: Vec<&str> = exts.iter().map(String::as_str).collect();

    // A key already in the environment (scripts, tests) skips the sign-in and the prompt.
    let preset = key_env(provider)
        .and_then(|k| std::env::var(k).ok())
        .filter(|t| !t.is_empty());
    let token = match preset {
        Some(t) => Some(t),
        None if provider == "claude-code" => {
            println!("Sign in with your Claude subscription.");
            // The token passes through `TokenMask` on its way to the terminal, so it is never shown.
            let mut child = h
                .exec_agent(&["-it"], &[], &[CLAUDE_CLI, "setup-token"])
                .stdout(Stdio::piped())
                .spawn()
                .context("running claude setup-token")?;
            let mut out = child.stdout.take().context("stdout")?;
            let mut mask = TokenMask::default();
            let mut buf = [0u8; 4096];
            let mut term = std::io::stdout();
            loop {
                let n = std::io::Read::read(&mut out, &mut buf)?;
                if n == 0 {
                    break;
                }
                term.write_all(&mask.feed(&buf[..n]))?;
                term.flush()?;
            }
            term.write_all(&mask.rest())?;
            if !child.wait()?.success() {
                bail!("claude setup-token failed");
            }
            match mask.token() {
                Some(t) => Some(t),
                None => Some(
                    Password::new()
                        .with_prompt("Paste the token it printed (hidden)")
                        .interact()?,
                ),
            }
        }
        None => None,
    };
    if let Some(token) = token {
        // rpi reads keys from a terminal, so `script` gives it one. The pty echoes the key back,
        // so the output is dropped; `auth check` tells whether it was stored.
        let login = rpi(&exts, &["auth", "login", "--provider", provider]).join(" ");
        feed(
            &mut h.exec_agent(&["-i"], &env, &["script", "-qec", &login, "/dev/null"]),
            token.trim(),
        )?;
        let check = output(&mut h.exec_agent(
            &[],
            &env,
            &rpi(&exts, &["auth", "check", "--json", "--provider", provider]),
        ))?;
        if !check.contains("\"ready\":true") {
            bail!("the token was not stored for {name}");
        }
    } else {
        run(&mut h.exec_agent(
            &["-it"],
            &env,
            &rpi(&exts, &["auth", "login", "--provider", provider]),
        ))?;
    }

    let current = conf::accounts(&doc)
        .into_iter()
        .find(|(n, _)| *n == name)
        .map(|(_, m)| m);
    let model = match model.or(current) {
        Some(m) => m,
        None => pick_model(h, &exts, Some(provider))?,
    };
    conf::upsert_account(&mut doc, &name, &model);
    h.data.save(&doc)?;
    println!("account {name}: {model}");
    if verify {
        println!("checking {name} with a test call...");
        let out = h
            .exec_agent(
                &[],
                &env,
                &rpi(
                    &exts,
                    &[
                        "-p",
                        "--no-session",
                        "--no-tools",
                        "--no-context-files",
                        "--no-skills",
                        "--model",
                        &model,
                        "Reply with the single word ok.",
                    ],
                ),
            )
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            println!("ok   test call through {name}");
        } else {
            // The last stderr line names the problem; output never carries the stored credential.
            let err = String::from_utf8_lossy(&out.stderr);
            let last = err.lines().last().unwrap_or_default();
            println!(
                "FAIL test call through {name}: {}",
                last.chars().take(200).collect::<String>()
            );
        }
    }
    Ok(())
}

/// `rpi <argv> --extension ..`
fn rpi<'a>(exts: &[&'a str], argv: &[&'a str]) -> Vec<&'a str> {
    [&["rpi"], argv, exts].concat()
}

/// Models from `rpi --list-models` in the container, as `provider/model`.
fn list_models(h: &Host, exts: &[&str], provider: Option<&str>) -> anyhow::Result<Vec<String>> {
    let argv = [&["rpi", "--list-models"], exts].concat();
    let text = output(&mut h.exec_agent(&[], &[], &argv))?;
    Ok(text
        .lines()
        .skip(1)
        .filter_map(|l| {
            let mut cols = l.split_whitespace();
            let (p, m) = (cols.next()?, cols.next()?);
            provider
                .is_none_or(|want| want == p)
                .then(|| format!("{p}/{m}"))
        })
        .collect())
}

fn pick_model(h: &Host, exts: &[&str], provider: Option<&str>) -> anyhow::Result<String> {
    println!("loading the models...");
    let models = list_models(h, exts, provider)?;
    if models.is_empty() {
        bail!(
            "rpi lists no models{}; pass --model",
            provider.map(|p| format!(" for {p}")).unwrap_or_default()
        );
    }
    if !h.interactive {
        return Ok(models[0].clone());
    }
    let i = Select::new()
        .with_prompt("Model")
        .items(&models)
        .default(0)
        .interact()?;
    Ok(models[i].clone())
}

pub fn accounts(h: &Host, a: Args) -> anyhow::Result<()> {
    let rest = a.rest()?;
    let mut doc = h.data.load()?;
    match rest.first().map(String::as_str) {
        None | Some("list") => {
            for (name, model) in conf::accounts(&doc) {
                let signed = h.data.account(&name).join("auth.json").exists();
                println!(
                    "{name:<20} {model:<40} {}",
                    if signed {
                        "signed in"
                    } else {
                        "no credentials"
                    }
                );
            }
            return Ok(());
        }
        Some("order") if rest.len() > 1 => conf::order_accounts(&mut doc, &rest[1..])?,
        Some("remove") if rest.len() == 2 => {
            if !conf::remove_account(&mut doc, &rest[1]) {
                bail!("no account {}", rest[1]);
            }
            let dir = h.data.account(&rest[1]);
            if dir.exists() {
                std::fs::remove_dir_all(&dir)?;
            }
        }
        _ => bail!("usage: zephyr accounts [list | order NAME.. | remove NAME]"),
    }
    h.data.save(&doc)?;
    h.apply()
}

/// Sets the default model, and the only account's model when there is exactly one.
pub fn model(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let account = a.value("--account")?;
    let rest = a.rest()?;
    let mut doc = h.data.load()?;
    let accounts = conf::accounts(&doc);
    // In pool mode each account has its own model; the top-level one is unused.
    let account = match (account, &accounts[..]) {
        (Some(name), _) => Some(
            accounts
                .iter()
                .find(|(n, _)| *n == name)
                .with_context(|| format!("no account {name}; see `zephyr accounts list`"))?
                .clone(),
        ),
        (None, [one]) => Some(one.clone()),
        (None, []) => None,
        (None, _) => bail!("several accounts: zephyr model --account NAME [PROVIDER/MODEL]"),
    };
    let provider = account.as_ref().map(|(_, m)| provider_of(m).to_string());
    let model = match &rest[..] {
        [m] if m.contains('/') => m.clone(),
        [] => {
            h.need_running()?;
            let exts = conf::extension_args(&doc);
            let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
            pick_model(h, &exts, provider.as_deref())?
        }
        _ => bail!("usage: zephyr model [--account NAME] [PROVIDER/MODEL]"),
    };
    match account {
        Some((name, _)) => {
            // The account's sign-in belongs to one provider.
            if provider.as_deref() != Some(provider_of(&model)) {
                bail!(
                    "{name} is signed in to {}; pick one of its models",
                    provider.unwrap_or_default()
                );
            }
            conf::upsert_account(&mut doc, &name, &model);
            if accounts.len() == 1 {
                conf::set(&mut doc, "model", &model)?;
            }
            println!("account {name}: {model}");
        }
        None => {
            conf::set(&mut doc, "model", &model)?;
            println!("model {model}");
        }
    }
    h.data.save(&doc)?;
    h.apply()
}

pub fn channel(h: &Host, a: Args) -> anyhow::Result<()> {
    match &a.rest()?[..] {
        [add, kind] if add == "add" && (kind == "discord" || kind == "imessage") => {
            channel_add(h, kind)?;
            h.apply()?;
            run(&mut h.exec(&[], &[], &["zephyr", "doctor"]))
        }
        [rm, kind] if rm == "remove" && (kind == "discord" || kind == "imessage") => {
            channel_remove(h, kind)
        }
        [list] if list == "list" => channel_list(h),
        _ => bail!("usage: zephyr channel add|remove discord|imessage | list"),
    }
}

/// `chat`, then each configured channel, with its owners.
fn channel_list(h: &Host) -> anyhow::Result<()> {
    let doc = h.data.load()?;
    for kind in ["chat", "discord", "imessage"] {
        if kind == "chat" || doc.get(kind).is_some() {
            let owners = conf::get(&doc, &format!("owners.{kind}")).unwrap_or_default();
            println!("{kind:<9}owners: {owners}");
        }
    }
    Ok(())
}

/// Removes a channel's table, its owners and the secrets `channel_add` wrote for it.
fn channel_remove(h: &Host, kind: &str) -> anyhow::Result<()> {
    let mut doc = h.data.load()?;
    if !conf::remove_channel(&mut doc, kind) {
        bail!("{kind} is not configured");
    }
    // Config first: a daemon reloading between the writes must not see a secret gone while its
    // channel is still configured.
    h.data.save(&doc)?;
    let secrets: &[&str] = if kind == "discord" {
        &["ZEPHYR_DISCORD_TOKEN"]
    } else {
        &["ZEPHYR_BB_PASSWORD", "ZEPHYR_BB_SECRET"]
    };
    for key in secrets {
        h.data.unset_secret(key)?;
    }
    println!("removed {kind}");
    h.apply()
}

/// `zephyr skill add DIR | list | rm NAME`, carried out by the container's `zephyr skill`
/// (DESIGN §4.6). `add` checks the folder as an approval would, then sends it over as a tar; the
/// owner adding it is the approval.
pub fn skill(h: &Host, a: Args) -> anyhow::Result<()> {
    let rest = a.rest()?;
    h.need_running()?;
    match &rest[..] {
        [add, dir] if add == "add" => {
            let dir = std::path::Path::new(dir).canonicalize()?;
            let name = dir
                .file_name()
                .and_then(|n| n.to_str())
                .context("the folder has no name")?
                .to_string();
            crate::drafts::read_skill(&dir, &name)
                .with_context(|| format!("{} is not a skill zephyr takes", dir.display()))?;
            let mut tar = std::process::Command::new("tar")
                .arg("-C")
                .arg(&dir)
                .args(["-cf", "-", "."])
                .stdout(Stdio::piped())
                .spawn()
                .context("running tar")?;
            let archive = tar.stdout.take().context("tar's output")?;
            run(h
                .exec(&["-i"], &[], &["zephyr", "skill", "add", &name])
                .stdin(archive))?;
            if !tar.wait()?.success() {
                bail!("tar failed");
            }
            Ok(())
        }
        [list] if list == "list" => run(&mut h.exec(&[], &[], &["zephyr", "skill", "list"])),
        [rm, name] if rm == "rm" => run(&mut h.exec(&[], &[], &["zephyr", "skill", "rm", name])),
        _ => bail!("usage: zephyr skill add DIR | list | rm NAME"),
    }
}

/// `zephyr packages add NAME.. | rm NAME.. | list`: apt packages for the agent, kept in host.toml
/// and baked into a local image on the release one, which `up` and `upgrade` build and run. The
/// image is built before the list is saved, so a name apt does not know changes nothing.
pub fn packages(h: &Host, a: Args) -> anyhow::Result<()> {
    const USE: &str = "usage: zephyr packages add NAME.. | rm NAME.. | list";
    let rest = a.rest()?;
    let mut list = h.data.packages()?;
    let before = list.clone();
    let Some((op, names)) = rest.split_first() else {
        bail!(USE);
    };
    match (op.as_str(), names.is_empty()) {
        ("list", true) => {
            if list.is_empty() {
                println!("no packages (zephyr packages add NAME)");
            }
            for p in &list {
                println!("{p}");
            }
            return Ok(());
        }
        ("add", false) => {
            if let Some(bad) = names.iter().find(|n| !super::package_name(n)) {
                bail!("{bad:?} is not an apt package name");
            }
            for n in names {
                if !list.contains(n) {
                    list.push(n.clone());
                }
            }
        }
        ("rm", false) => {
            if let Some(gone) = names.iter().find(|n| !list.contains(n)) {
                bail!("{gone} is not in the list (zephyr packages list)");
            }
            list.retain(|p| !names.contains(p));
        }
        _ => bail!(USE),
    }
    list.sort();
    if list == before {
        println!("already there; nothing to do");
        return Ok(());
    }
    h.ensure_base(false, false)?;
    h.derive(&list, false)?;
    h.data.set_packages(&list)?;
    if list.is_empty() {
        println!("no packages: zephyr runs {} again", h.image);
    } else {
        println!("packages: {}", list.join(" "));
    }
    if !h.running() {
        println!("saved; applies when zephyr starts (`zephyr up`)");
        return Ok(());
    }
    // Recreated as an upgrade is: once nothing is in flight.
    h.settle(super::running_version(h).as_deref())?;
    h.up()
}

/// `zephyr mcp add NAME (--url URL [--header K=V].. | -- CMD ARGS..) [--secret NAME].. | list |
/// rm NAME`: the `[[mcp]]` connectors in zephyr.toml (DESIGN §4.6). Each `--secret` is asked for
/// hidden and goes to secrets.env, for the `${NAME}` it fills in. `add NAME --secret K` alone sets
/// a secret for a connector that is already there.
pub fn mcp(h: &Host, mut a: Args) -> anyhow::Result<()> {
    use crate::config::{Config, Mcp};
    // After `--` comes the server's own command line, options and all.
    let command = match a.0.iter().position(|x| x == "--") {
        Some(i) => a.0.split_off(i).split_off(1),
        None => Vec::new(),
    };
    let url = a.value("--url")?;
    let headers = a.values("--header")?;
    let secrets = a.values("--secret")?;
    let rest = a.rest()?;
    let mut doc = h.data.load()?;
    let parse = |doc: &toml_edit::DocumentMut| -> anyhow::Result<Config> {
        let cfg: Config = toml::from_str(&doc.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    };
    let usage = "usage: zephyr mcp add NAME (--url URL [--header K=V].. | -- CMD ARGS..) [--secret NAME].. | list | rm NAME";
    match &rest[..] {
        [list] if list == "list" => {
            for m in parse(&doc)?.mcp {
                println!("{}: {}", m.name, m.url.or(m.command).unwrap_or_default());
            }
            return Ok(());
        }
        [rm, name] if rm == "rm" => {
            if !conf::remove_mcp(&mut doc, name) {
                bail!("no connector named {name}");
            }
        }
        [add, name] if add == "add" => {
            let headers = headers
                .iter()
                .map(|h| {
                    h.split_once('=')
                        .map(|(k, v)| (k.trim().to_string(), v.trim().to_string()))
                        .with_context(|| format!("--header {h}: write it as K=V"))
                })
                .collect::<anyhow::Result<_>>()?;
            let m = match (url, command.split_first()) {
                (Some(_), Some(_)) => bail!("give --url or a command after --, not both"),
                (Some(url), None) => Mcp {
                    name: name.clone(),
                    url: Some(url),
                    headers,
                    ..Mcp::default()
                },
                (None, Some((cmd, args))) if headers.is_empty() => Mcp {
                    name: name.clone(),
                    command: Some(cmd.clone()),
                    args: args.to_vec(),
                    ..Mcp::default()
                },
                (None, Some(_)) => bail!("--header goes with --url"),
                (None, None) => parse(&doc)?
                    .mcp
                    .into_iter()
                    .find(|m| m.name == *name)
                    .with_context(|| format!("no connector named {name}\n{usage}"))?,
            };
            conf::upsert_mcp(&mut doc, &m)?;
        }
        _ => bail!("{usage}"),
    }
    parse(&doc)?;
    if !secrets.is_empty() && !h.interactive {
        bail!("--secret asks for the value: run it in a terminal");
    }
    for key in &secrets {
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            bail!("--secret {key:?}: use a variable name (letters, digits, '_')");
        }
        let value = Password::new()
            .with_prompt(format!("{key} (hidden)"))
            .interact()?;
        h.data.set_secret(key, value.trim())?;
    }
    h.data.save(&doc)?;
    println!("saved {}", rest.join(" "));
    h.apply()
}

/// `zephyr vault list | add login NAME --site URL [--user U] [--totp] | add token NAME |
/// edit NAME [--site URL] [--user U] [--totp] | rm NAME | revoke NAME [PROGRAM]`: `vault.toml`,
/// one of the daemon's files (DESIGN §4.15), which the daemon reads on every use. Values are
/// asked at hidden prompts, or read one per line from stdin when it is not a terminal; never
/// taken as arguments. Grants are only the owner's `/ok` or `/vault grant` in a chat. A save here
/// is not told to the agent: the owner tells it.
pub fn vault(h: &Host, mut a: Args) -> anyhow::Result<()> {
    use crate::vault::{Entry, Kind, Vault};
    let site = a.value("--site")?;
    let user = a.value("--user")?;
    let totp = a.flag("--totp");
    let rest = a.rest()?;
    let usage = "usage: zephyr vault list | add login NAME --site URL [--user U] [--totp] | \
                 add token NAME | edit NAME [--site URL] [--user U] [--totp] | rm NAME | \
                 revoke NAME [PROGRAM]";
    let text = match h.data.read(crate::vault::FILE) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        r => r?,
    };
    let mut vault = Vault::parse(&text)?;
    let mut lines = std::io::stdin().lines();
    let mut secret = |prompt: &str| -> anyhow::Result<String> {
        if h.interactive {
            let p = Password::new().with_prompt(format!("{prompt} (hidden)"));
            return Ok(p.allow_empty_password(true).interact()?);
        }
        Ok(lines.next().transpose()?.unwrap_or_default())
    };
    let args: Vec<&str> = rest.iter().map(String::as_str).collect();
    let said = match args[..] {
        [] | ["list"] => {
            let entries = vault.entries();
            if entries.is_empty() {
                println!("the vault is empty");
            }
            entries.iter().for_each(|e| println!("{}", e.line()));
            return Ok(());
        }
        ["add", "login", name] => {
            let site = site.with_context(|| format!("add login needs --site URL\n{usage}"))?;
            let password = secret(&format!("password for {name}"))?;
            let totp = match totp {
                true => secret("TOTP seed (BASE32)")?,
                false => String::new(),
            };
            vault.put(Entry {
                kind: Kind::Login,
                name: name.into(),
                site,
                username: user.unwrap_or_default(),
                password,
                totp,
                ..Entry::default()
            })?;
            format!("saved the login {name}; the agent asks you before its first fill")
        }
        ["add", "token", name] => {
            let value = secret(name)?;
            vault.put(Entry {
                kind: Kind::Token,
                name: name.into(),
                value,
                ..Entry::default()
            })?;
            format!("saved the token {name}; the agent asks you before each new program uses it")
        }
        ["edit", name] => {
            let mut e = vault
                .get(name)
                .with_context(|| format!("no entry named {name} in the vault"))?;
            // An empty answer keeps what is saved.
            match e.kind {
                Kind::Login => {
                    e.site = site.unwrap_or(e.site);
                    e.username = user.unwrap_or(e.username);
                    e.password = secret("new password, empty keeps it")?;
                    e.totp = match totp {
                        true => secret("new TOTP seed, empty keeps it")?,
                        false => String::new(),
                    };
                }
                Kind::Token => e.value = secret("new value, empty keeps it")?,
            }
            vault.put(e)?;
            format!("saved {name}")
        }
        ["rm", name] => match vault.remove(name) {
            true => format!("removed {name}"),
            false => bail!("no entry named {name} in the vault"),
        },
        ["revoke", name] => vault.revoke(name, None)?,
        ["revoke", name, program] => vault.revoke(name, Some(program))?,
        _ => bail!("{usage}"),
    };
    h.data.write(crate::vault::FILE, &vault.to_string())?;
    println!("{said}");
    Ok(())
}

/// `zephyr subagent list | add NAME [--model P/M] [--prompt TEXT] | edit NAME [--model
/// P/M|default] [--prompt TEXT] | rm NAME`: the owner's `[[subagent]]`s in zephyr.toml (DESIGN
/// §4.12). Without `--prompt`, `add` and `edit` read the prompt from stdin when it is not a
/// terminal.
pub fn subagent(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let model = a.value("--model")?;
    let mut prompt = a.value("--prompt")?;
    let rest = a.rest()?;
    let usage = "usage: zephyr subagent list | add NAME [--model P/M] [--prompt TEXT] | \
                 edit NAME [--model P/M|default] [--prompt TEXT] | rm NAME";
    let mut doc = h.data.load()?;
    let args: Vec<&str> = rest.iter().map(String::as_str).collect();
    if prompt.is_none() && matches!(args[..], ["add" | "edit", _]) && !h.interactive {
        let text = std::io::read_to_string(std::io::stdin())?;
        prompt = Some(text.trim().to_string()).filter(|t| !t.is_empty());
    }
    let said = match args[..] {
        [] | ["list"] => {
            let cfg: crate::config::Config = toml::from_str(&doc.to_string())?;
            println!("{}", crate::tasks::settings(&cfg));
            return Ok(());
        }
        ["add", name] => {
            let prompt = prompt.with_context(|| {
                format!("add needs --prompt TEXT, or the prompt on stdin\n{usage}")
            })?;
            conf::set_subagent(&mut doc, name, true, model.as_deref(), Some(&prompt))?;
            format!("added the subagent {name}")
        }
        ["edit", name] if model.is_some() || prompt.is_some() => {
            conf::set_subagent(&mut doc, name, false, model.as_deref(), prompt.as_deref())?;
            format!("saved the subagent {name}")
        }
        ["rm", name] => match conf::remove_subagent(&mut doc, name) {
            true => format!("removed the subagent {name}"),
            false => bail!("no subagent named {name}"),
        },
        _ => bail!("{usage}"),
    };
    // Refuse what the daemon would refuse (a bad name or model, an empty prompt).
    toml::from_str::<crate::config::Config>(&doc.to_string())
        .map_err(anyhow::Error::from)
        .and_then(|c| c.validate())
        .context("not saved")?;
    h.data.save(&doc)?;
    println!("{said}");
    h.apply()
}

/// Asks for the channel's settings; secrets go to secrets.env at hidden prompts.
fn channel_add(h: &Host, kind: &str) -> anyhow::Result<()> {
    if !h.interactive {
        bail!("channel add asks for secrets: run it in a terminal");
    }
    let mut doc = h.data.load()?;
    let ask = |prompt: &str, key: &str| -> anyhow::Result<String> {
        let mut input = Input::<String>::new().with_prompt(prompt);
        if let Some(v) = conf::get(&doc, key) {
            input = input.default(v);
        }
        Ok(input.interact_text()?)
    };
    let hidden = |prompt: &str, env: &str| -> anyhow::Result<()> {
        let v = Password::new().with_prompt(prompt).interact()?;
        h.data.set_secret(env, v.trim())
    };
    let list = |v: &str| {
        let items: Vec<String> = v.split(',').map(|s| format!("{:?}", s.trim())).collect();
        format!("[{}]", items.join(", "))
    };
    if kind == "discord" {
        let owner = ask(
            "Your Discord user id (Developer Mode, Copy User ID)",
            "owners.discord",
        )?;
        hidden("Discord bot token (hidden)", "ZEPHYR_DISCORD_TOKEN")?;
        conf::set(&mut doc, "discord.token_env", "ZEPHYR_DISCORD_TOKEN")?;
        conf::set(&mut doc, "owners.discord", &list(&owner))?;
    } else {
        let listen = ask(
            "Webhook listen address (this host's Tailscale IP:port)",
            "imessage.listen",
        )?;
        let server = ask("BlueBubbles server URL", "imessage.server_url")?;
        let owners = ask("Your iMessage handles, comma separated", "owners.imessage")?;
        hidden("BlueBubbles server password (hidden)", "ZEPHYR_BB_PASSWORD")?;
        h.data
            .set_secret("ZEPHYR_BB_SECRET", &conf::random_hex(24)?)?;
        conf::set(&mut doc, "imessage.listen", &format!("{listen:?}"))?;
        conf::set(&mut doc, "imessage.server_url", &format!("{server:?}"))?;
        conf::set(&mut doc, "imessage.password_env", "ZEPHYR_BB_PASSWORD")?;
        conf::set(&mut doc, "imessage.secret_env", "ZEPHYR_BB_SECRET")?;
        conf::set(&mut doc, "owners.imessage", &list(&owners))?;
        println!(
            "Register the BlueBubbles webhook http://{listen}/?secret=<ZEPHYR_BB_SECRET from {}/secrets.env, readable with sudo>",
            h.data.daemon.display()
        );
    }
    h.data.save(&doc)
}

const TOKEN_PREFIX: &[u8] = b"sk-ant-oat01-";
/// The screen may redraw the next line by moving the cursor, so the length is what ends a token.
/// shortcut: every token seen so far is 108 bytes; a new format shows as a failed test call.
const TOKEN_LEN: usize = 108;

/// Replaces each character of a Claude token in terminal output with `*` and keeps the token.
/// The terminal may wrap it, so escapes and single line breaks inside it pass through; a blank
/// line or any other character ends it.
#[derive(Default)]
struct TokenMask {
    /// Bytes matching the start of TOKEN_PREFIX, held until they match or not.
    held: Vec<u8>,
    token: Option<Vec<u8>>,
    /// In a token: line breaks since its last character, and the escape-sequence state.
    masking: bool,
    breaks: u8,
    escape: u8,
}

impl TokenMask {
    fn feed(&mut self, chunk: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(chunk.len());
        for &b in chunk {
            if self.masking {
                out.push(match (self.escape, b) {
                    (1, _) => {
                        self.escape = if b == b'[' { 2 } else { 0 };
                        b
                    }
                    (2, _) => {
                        if (0x40..=0x7e).contains(&b) {
                            self.escape = 0;
                        }
                        b
                    }
                    (_, 0x1b) => {
                        self.escape = 1;
                        b
                    }
                    (_, b'\n') => {
                        self.breaks += 1;
                        self.masking = self.breaks < 2;
                        b
                    }
                    (_, b' ' | b'\r' | b'\t') => b,
                    (_, c) if c.is_ascii_alphanumeric() || c == b'-' || c == b'_' => {
                        self.breaks = 0;
                        if let Some(t) = &mut self.token {
                            t.push(c);
                            self.masking = t.len() < TOKEN_LEN;
                        }
                        b'*'
                    }
                    _ => {
                        self.masking = false;
                        b
                    }
                });
                continue;
            }
            self.held.push(b);
            if self.held == TOKEN_PREFIX {
                out.append(&mut self.held);
                self.token = Some(TOKEN_PREFIX.to_vec());
                (self.masking, self.breaks, self.escape) = (true, 0, 0);
            } else if !TOKEN_PREFIX.starts_with(&self.held) {
                // 's' starts the prefix and appears nowhere else in it, so only `b` can restart it.
                let last = self.held.pop();
                out.append(&mut self.held);
                if last == Some(TOKEN_PREFIX[0]) {
                    self.held.push(TOKEN_PREFIX[0]);
                } else {
                    out.extend(last);
                }
            }
        }
        out
    }

    /// Bytes still held at the end of the output.
    fn rest(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.held)
    }

    /// The last token seen, if it has more than the prefix.
    fn token(&self) -> Option<String> {
        let t = self
            .token
            .as_ref()
            .filter(|t| t.len() > TOKEN_PREFIX.len())?;
        String::from_utf8(t.clone()).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::TokenMask;

    fn run(chunks: &[&str]) -> (String, Option<String>) {
        let mut m = TokenMask::default();
        let mut shown: Vec<u8> = chunks.iter().flat_map(|c| m.feed(c.as_bytes())).collect();
        shown.extend(m.rest());
        (String::from_utf8(shown).unwrap(), m.token())
    }

    #[test]
    fn hides_the_token_and_keeps_it() {
        let (shown, token) = run(&[
            "\x1b[1m Your OAuth token:\x1b[22m\r\n\r\n sk-ant-oat01-Ab_c-9\x1b[0m\r\n\r\n Store this",
        ]);
        assert_eq!(token.as_deref(), Some("sk-ant-oat01-Ab_c-9"));
        assert_eq!(shown, "\x1b[1m Your OAuth token:\x1b[22m\r\n\r\n sk-ant-oat01-******\x1b[0m\r\n\r\n Store this");
    }

    #[test]
    fn a_wrapped_token_split_across_reads_is_hidden_whole() {
        let (shown, token) = run(&[
            "x ssk-ant-o",
            "at01-Ab_c\r\n",
            " -9xy\x1b[39m\r\n\r\n Store",
        ]);
        assert_eq!(token.as_deref(), Some("sk-ant-oat01-Ab_c-9xy"));
        assert!(!shown.contains("Ab") && !shown.contains("9xy"), "{shown}");
        assert!(
            shown.starts_with("x s") && shown.ends_with(" Store"),
            "{shown}"
        );
    }

    #[test]
    fn a_full_length_token_ends_where_the_screen_moves_on() {
        let token = format!("sk-ant-oat01-{}AA", "x".repeat(super::TOKEN_LEN - 15));
        let (shown, kept) = run(&[&format!("{token}\x1b[2B\x1b[2GStore\x1b[8Gthis")]);
        assert_eq!(kept, Some(token));
        assert!(shown.ends_with("\x1b[2B\x1b[2GStore\x1b[8Gthis"), "{shown}");
    }

    #[test]
    fn output_without_a_token_passes_through() {
        assert_eq!(
            run(&["Login cancelled\r\n"]),
            ("Login cancelled\r\n".into(), None)
        );
    }
}
