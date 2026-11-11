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
    let preset = a.value("--preset")?;
    let network = a.value("--network")?;
    let rest = a.rest()?;
    if !rest.is_empty() {
        bail!("{USAGE}");
    }
    let preset = preset
        .map(|p| crate::approvals::Preset::parse(&p))
        .transpose()
        .map_err(|e| anyhow::anyhow!("--preset: {e}"))?;
    // No prompt for it: the floor is the safe default, and a wizard should not offer the weaker one.
    if network
        .as_deref()
        .is_some_and(|n| n != "bridge" && n != "host")
    {
        bail!("--network must be bridge or host");
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
    h.data.set_network(network.as_deref())?;
    println!(
        "{}",
        match h.bridge()? {
            true => "network: bridge (the agent's network floor)",
            false => "network: host (no network floor; zephyr init --network bridge turns it on)",
        }
    );

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
    // Without --preset, [approvals] stays absent, which is `manual`; the interactive run asks.
    if let Some(p) = preset.filter(|_| non_interactive) {
        write_preset(&mut doc, p)?;
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
        if doc.get("discord").is_none() && doc.get("imessage").is_none() {
            println!("  add a channel: zephyr channel add discord|imessage");
        }
        if conf::get(&doc, "web.host").is_none() && h.bridge()? {
            println!("  web app: zephyr web setup");
        }
        if !no_start {
            h.ensure_image(false, false)?;
            h.up_checked()?;
            h.doctor(false)?;
        }
        return Ok(());
    }

    // Logins run in the container, so it starts before them.
    h.ensure_image(false, false)?;
    h.up_checked()?;
    // A restart cannot publish a port added from here on (the iMessage webhook): see the end.
    let published = h.published()?;
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
    let mut paired = vec![];
    for kind in CHANNELS {
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
            paired.extend(PAIRED.iter().filter(|k| **k == kind));
        }
    }
    init_approvals(h, preset)?;
    // After the preset, so web setup's strong-channel and money offers come after it.
    use super::web::Restart;
    match init_web(h)? {
        // web setup made the container again, with every port.
        Restart::Done => {}
        Restart::Declined => {
            println!("not starting zephyr again, as you asked; zephyr up applies the changes");
        }
        // A restart cannot publish a port this run added; `republish` makes it again for that.
        Restart::NotAsked => {
            if !h.republish(&published)? {
                run(&mut super::docker(&["restart", &h.container]))?;
            }
        }
    }
    h.doctor(false)?;
    for kind in paired {
        pair(h, kind)?;
    }
    Ok(())
}

/// The channels `channel add` sets up, in the order `init` offers them.
const CHANNELS: [&str; 4] = ["discord", "imessage", "telegram", "signal"];
/// Those whose owner is learned by a pairing code rather than typed in (DESIGN §4.3).
const PAIRED: [&str; 2] = ["telegram", "signal"];
const CHANNEL_USAGE: &str =
    "usage: zephyr channel add|remove discord|imessage|telegram|signal | list | signal trust ACI";

/// Asks the daemon for a pairing code for `kind` and prints how to use it: `zephyr pair` in
/// the container, over the chat socket, a strong channel.
fn pair(h: &Host, kind: &str) -> anyhow::Result<()> {
    run(&mut h.exec(&[], &[], &["zephyr", "pair", kind]))
}

/// Init's web app step (DESIGN §4.8): offered, default no. What web setup did about restarting
/// zephyr; an error is printed and init carries on, as a failed sign-in does.
fn init_web(h: &Host) -> anyhow::Result<super::web::Restart> {
    use super::web::Restart;
    if !h.bridge()? {
        println!("web app: {}", crate::web::NEEDS_FLOOR);
        return Ok(Restart::NotAsked);
    }
    let prompt = match conf::get(&h.data.load()?, "web.host") {
        Some(host) => {
            println!("web app: https://{host}");
            "Change the web app?"
        }
        None => "Set up the web app (needs Tailscale with HTTPS certificates)?",
    };
    if !Confirm::new()
        .with_prompt(prompt)
        .default(false)
        .interact()?
    {
        return Ok(Restart::NotAsked);
    }
    let mut restart = Restart::NotAsked;
    if let Err(e) = super::web::setup(h, false, &mut restart) {
        println!("web app: {e:#}");
    }
    Ok(restart)
}

/// Writes `preset` as `/approvals PRESET` does (D11, `approvals::preset_edits`): the preset, and
/// every hand-set class looser than it cleared, so a re-run of `init --preset manual` never
/// leaves a looser class behind.
fn write_preset(
    doc: &mut toml_edit::DocumentMut,
    preset: crate::approvals::Preset,
) -> anyhow::Result<()> {
    let cfg: crate::config::Config =
        toml::from_str(&doc.to_string()).context("reading zephyr.toml")?;
    for (key, value) in crate::approvals::preset_edits(&cfg, preset) {
        let value = value.map(toml_edit::Value::from);
        crate::settings::write(doc, &key, value.as_ref())?;
    }
    Ok(())
}

/// The preset to start in (approvals spec §4.10), then the private facts. A config that already
/// has an `[approvals]` table (a re-run of `init`) keeps it unless `--preset` says otherwise.
fn init_approvals(h: &Host, preset: Option<crate::approvals::Preset>) -> anyhow::Result<()> {
    use crate::approvals::Preset;
    let mut doc = h.data.load()?;
    let chosen = match preset {
        Some(p) => Some(p),
        None if doc.get("approvals").is_some() => None,
        None => {
            let presets = [Preset::Manual, Preset::Semi, Preset::Full];
            let items: Vec<String> = (presets.iter())
                .map(|p| format!("{}: {}", p.name(), p.summary()))
                .collect();
            let i = Select::new()
                .with_prompt("Approvals: how much may the agent do without asking you")
                .items(&items)
                .default(0)
                .interact()?;
            Some(presets[i])
        }
    };
    if let Some(p) = chosen {
        write_preset(&mut doc, p)?;
        h.data.save(&doc)?;
    }
    if Confirm::new()
        .with_prompt(
            "Add private facts (a phone number, an address) the agent may not send unasked?",
        )
        .default(false)
        .interact()?
    {
        loop {
            if let Err(e) = add_fact(h) {
                println!("not saved: {e:#}");
            }
            if !Confirm::new()
                .with_prompt("Add another fact?")
                .default(false)
                .interact()?
            {
                break;
            }
        }
        println!("allow a fact to go to someone with: zephyr facts allow NAME TARGET");
    }
    Ok(())
}

/// One fact, asked for: its name, kind and value (hidden), saved as `zephyr facts add` does.
fn add_fact(h: &Host) -> anyhow::Result<()> {
    use crate::facts::FactKind;
    let name: String = Input::new()
        .with_prompt("Fact name (e.g. phone)")
        .interact_text()?;
    let kinds = ["text", "digits", "email"];
    let k = Select::new()
        .with_prompt("Kind")
        .items(kinds)
        .default(0)
        .interact()?;
    let value = Password::new()
        .with_prompt(format!("value of {name} (hidden)"))
        .allow_empty_password(true)
        .interact()?;
    let kind = FactKind::parse(kinds[k]).unwrap_or_default();
    println!("{}", save_fact(h, &name, kind, &value)?);
    Ok(())
}

/// Saves one fact in facts.toml the way `zephyr facts add` does: the name trimmed, an existing
/// fact of that name keeping its allowed targets. Returns what to tell the owner.
fn save_fact(
    h: &Host,
    name: &str,
    kind: crate::facts::FactKind,
    value: &str,
) -> anyhow::Result<String> {
    use crate::facts::{parse, put, render, Fact};
    let name = name.trim();
    let text = match h.data.read(crate::facts::FILE) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        r => r?,
    };
    let mut facts = parse(&text)?;
    let allow = (facts.iter().find(|f| f.name == name)).map_or_else(Vec::new, |f| f.allow.clone());
    let fact = Fact {
        name: name.into(),
        kind,
        value: value.trim().into(),
        allow,
    };
    let new = put(&mut facts, fact, true)?;
    h.data.write(crate::facts::FILE, &render(&text, &facts)?)?;
    // D12: what a call holding it does now, as `approvals.facts` says.
    let cfg = h.data.load().ok();
    let cfg = cfg.and_then(|d| toml::from_str::<crate::config::Config>(&d.to_string()).ok());
    let mode = cfg.map(|c| crate::approvals::effective(&c).facts);
    let then = match mode {
        Some(crate::approvals::FactsMode::Hold) => "; calls holding it are held for your OK",
        Some(crate::approvals::FactsMode::Warn) => "; calls holding it go, and you are told",
        Some(crate::approvals::FactsMode::Off) => {
            "; it is not looked for while approvals.facts is off"
        }
        None => "",
    };
    Ok(match new {
        true => format!("saved the fact {name}{then}"),
        false => format!("replaced the value of {name}"),
    })
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
    let doc = h.data.load()?;
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
        if h.floored() {
            // The provider's callback to 127.0.0.1 does not reach the container (DESIGN §4.20).
            println!("if the browser then cannot open 127.0.0.1, paste its address here");
        }
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
    // Read again: the sign-in and the picker take minutes, and the daemon may have edited it since.
    let mut doc = h.data.load()?;
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
    let doc = h.data.load()?;
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
    // Read again: the picker can take minutes, and the daemon may have edited it since.
    let mut doc = h.data.load()?;
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
        [add, kind] if add == "add" && CHANNELS.contains(&kind.as_str()) => {
            let before = h.published().unwrap_or_default();
            channel_add(h, kind)?;
            h.apply_published(&before)?;
            // The pairing code first (`zephyr pair` waits for the channel to come up), then
            // doctor, whose failure fails the command as before.
            if PAIRED.contains(&kind.as_str()) {
                pair(h, kind)?;
            }
            run(&mut h.exec(&[], &[], &["zephyr", "doctor"]))
        }
        [rm, kind] if rm == "remove" && CHANNELS.contains(&kind.as_str()) => {
            channel_remove(h, kind)
        }
        [list] if list == "list" => channel_list(h),
        // The daemon's signal-cli holds the account, so the trust goes through it (DESIGN §4.3).
        [signal, trust, aci] if signal == "signal" && trust == "trust" => {
            h.need_running()?;
            run(&mut h.exec(&[], &[], &["zephyr", "signal-setup", "trust", aci]))
        }
        _ => bail!("{CHANNEL_USAGE}"),
    }
}

/// `chat`, then each configured channel, with its owners.
fn channel_list(h: &Host) -> anyhow::Result<()> {
    let doc = h.data.load()?;
    for kind in ["chat"].into_iter().chain(CHANNELS) {
        if kind == "chat" || doc.get(kind).is_some() {
            let owners = conf::get(&doc, &format!("owners.{kind}")).unwrap_or_default();
            println!("{kind:<9}owners: {owners}");
        }
    }
    Ok(())
}

/// Removes a channel's table, its owners and the secrets `channel_add` wrote for it.
fn channel_remove(h: &Host, kind: &str) -> anyhow::Result<()> {
    let before = h.published().unwrap_or_default();
    let mut doc = h.data.load()?;
    if !conf::remove_channel(&mut doc, kind) {
        bail!("{kind} is not configured");
    }
    // Config first: a daemon reloading between the writes must not see a secret gone while its
    // channel is still configured.
    h.data.save(&doc)?;
    let secrets: &[&str] = match kind {
        "discord" => &["ZEPHYR_DISCORD_TOKEN"],
        "imessage" => &["ZEPHYR_BB_PASSWORD", "ZEPHYR_BB_SECRET"],
        "telegram" => &["ZEPHYR_TELEGRAM_TOKEN"],
        _ => &[],
    };
    for key in secrets {
        h.data.unset_secret(key)?;
    }
    println!("removed {kind}");
    h.apply_published(&before)?;
    if kind == "signal" {
        // After the reload: signal-cli waits for the daemon's child to let the account go.
        if !h.running() {
            println!(
                "the Signal account's keys stay in {}/signal until you run `zephyr up` and then \
                 `docker exec {} zephyr signal-setup remove`",
                h.data.daemon.display(),
                h.container
            );
            return Ok(());
        }
        run(&mut h.exec(&[], &[], &["zephyr", "signal-setup", "remove"]))?;
    }
    Ok(())
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
    // Where its secrets are sent (DESIGN §4.21), or `--plain` to hand it the real ones.
    let hosts = a.values("--host")?;
    let plain = a.flag("--plain");
    // A ready-made entry from the image's catalog (DESIGN §4.22).
    let catalog = a.value("--catalog")?;
    let rest = a.rest()?;
    // Whether this writes a connector's table by hand (not `add NAME --secret K` alone).
    let by_hand = url.is_some() || !command.is_empty();
    // The catalog entry's secrets secrets.env lacks, asked for (or named) once it is saved.
    let mut wanted: Vec<crate::catalog::Secret> = Vec::new();
    let mut doc = h.data.load()?;
    let parse = |doc: &toml_edit::DocumentMut| -> anyhow::Result<Config> {
        let cfg: Config = toml::from_str(&doc.to_string())?;
        cfg.validate()?;
        Ok(cfg)
    };
    let was = parse(&doc).ok();
    let usage = "usage: zephyr mcp add NAME (--url URL [--header K=V].. | -- CMD ARGS..) [--secret NAME].. [--host HOST].. [--plain] | add --catalog NAME [--url URL] [--host HOST].. [--plain] | catalog | list | rm NAME";
    // The image's copy of an entry, read as root by full path (`valid_name` first: it becomes a path).
    let entry_text = |name: &str| -> anyhow::Result<String> {
        if !crate::drafts::valid_name(name) {
            bail!("no catalog entry named {name:?}");
        }
        let path = format!("{}/{name}.toml", crate::catalog::DIR);
        output(&mut h.exec(&[], &[], &["/usr/bin/cat", &path])).with_context(|| {
            format!("no catalog entry named {name} (zephyr mcp catalog lists them)")
        })
    };
    match &rest[..] {
        [list] if list == "catalog" && catalog.is_none() => {
            h.need_running()?;
            let names = output(&mut h.exec(&[], &[], &["/usr/bin/ls", crate::catalog::DIR]))?;
            let mut names: Vec<&str> = names
                .lines()
                .filter_map(|n| n.strip_suffix(".toml"))
                .collect();
            names.sort();
            for name in names {
                let e = crate::catalog::parse(name, &entry_text(name)?)?;
                println!("{name}: {}", e.about);
            }
            return Ok(());
        }
        [add] if add == "add" && catalog.is_some() => {
            let name = catalog.as_deref().unwrap_or_default();
            h.need_running()?;
            let entry = crate::catalog::parse(name, &entry_text(name)?)?;
            let old = parse(&doc)
                .ok()
                .and_then(|c| c.mcp.into_iter().find(|m| m.name == name));
            // Only this add's `--plain`: an old table's is no consent for this one (DESIGN §4.22).
            if let Some(why) = entry.plain_needed(url.as_deref()).filter(|_| !plain) {
                bail!("{why}: add --plain to hand it the real key, which the agent can then read");
            }
            let m =
                crate::catalog::merged(entry.to_mcp(url.as_deref())?, old.as_ref(), &hosts, plain);
            if let Some(note) = crate::catalog::lost_plain(old.as_ref(), &m) {
                println!("{name}: {note}");
            }
            conf::upsert_mcp(&mut doc, &m)?;
            let set = crate::dotenv::parse(&h.data.read("secrets.env").unwrap_or_default());
            let lacks = |s: &crate::catalog::Secret| !set.iter().any(|(k, _)| *k == s.name);
            wanted = (entry.secrets.into_iter())
                .filter(|s| lacks(s) && !secrets.contains(&s.name))
                .collect();
        }
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
            // D4: added again, a connector keeps the classes the owner gave its tools.
            let existing = || {
                let cfg = parse(&doc).ok();
                cfg.and_then(|c| c.mcp.into_iter().find(|m| m.name == *name))
            };
            let classes = || existing().map(|m| m.classes).unwrap_or_default();
            // And where its secrets go, unless this call says.
            let old = existing().unwrap_or_default();
            let mut m = match (url, command.split_first()) {
                (Some(_), Some(_)) => bail!("give --url or a command after --, not both"),
                (Some(url), None) => Mcp {
                    name: name.clone(),
                    url: Some(url),
                    headers,
                    classes: classes(),
                    ..Mcp::default()
                },
                (None, Some((cmd, args))) if headers.is_empty() => Mcp {
                    name: name.clone(),
                    command: Some(cmd.clone()),
                    args: args.to_vec(),
                    classes: classes(),
                    ..Mcp::default()
                },
                (None, Some(_)) => bail!("--header goes with --url"),
                (None, None) => parse(&doc)?
                    .mcp
                    .into_iter()
                    .find(|m| m.name == *name)
                    .with_context(|| format!("no connector named {name}\n{usage}"))?,
            };
            m.hosts = match hosts.is_empty() {
                true => old.hosts,
                false => hosts.join(", "),
            };
            m.secrets = match plain {
                true => Some("plain".into()),
                false => old.secrets,
            };
            // A remote connector can only hold its real secret (rpi's own client cannot use a
            // stand-in, DESIGN §4.21), which the agent can then read: said here, not implied.
            if crate::broker::undecided(&m) {
                bail!(
                    "a remote connector's secret is handed to it as it is, where the agent can \
                     read it (rpi's own client cannot use a stand-in): add --plain to accept that"
                );
            }
            conf::upsert_mcp(&mut doc, &m)?;
        }
        _ => bail!("{usage}"),
    }
    let now = parse(&doc)?;
    // As `/set`: this may not make one secret a connector's own and another's stand-in.
    // With no config that read before, the new one alone must hold none.
    let made = match &was {
        Some(was) => crate::broker::creates_pair(was, &now),
        None => crate::broker::shared_notice(&now).map_or(Ok(()), Err),
    };
    made.map_err(anyhow::Error::msg)?;
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
    for s in &wanted {
        println!("{}: {}", s.name, s.help);
        if !h.interactive {
            let name = catalog.as_deref().unwrap_or_default();
            println!(
                "set it in a terminal with: zephyr mcp add {name} --secret {}",
                s.name
            );
            continue;
        }
        let value = Password::new()
            .with_prompt(format!("{} (hidden)", s.name))
            .interact()?;
        h.data.set_secret(&s.name, value.trim())?;
    }
    h.data.save(&doc)?;
    // Which connectors came from the catalog, for doctor's note (DESIGN §4.22): the daemon's
    // file, never zephyr.toml.
    let from_catalog = match (&catalog, &rest[..]) {
        (Some(name), _) => Some((name.as_str(), true)),
        (None, [verb, name]) if verb == "rm" || (verb == "add" && by_hand) => {
            Some((name.as_str(), false))
        }
        _ => None,
    };
    if let Some((name, on)) = from_catalog {
        let text = h.data.read(crate::catalog::ADDED).unwrap_or_default();
        let new = crate::catalog::with_added(&text, name, on);
        if new != text && (on || !text.is_empty()) {
            // Only doctor's note hangs on it: an older image's `__file` refuses the name.
            if let Err(e) = h.data.write(crate::catalog::ADDED, &new) {
                eprintln!("warn {e:#}: zephyr doctor may not say when a newer catalog entry ships");
            }
        }
    }
    match &catalog {
        Some(name) => println!("saved the catalog connector {name}"),
        None => println!("saved {}", rest.join(" ")),
    }
    h.apply()
}

/// `zephyr vault list | add login NAME --site URL [--user U] [--totp] | add token NAME
/// [--host HOST].. | edit NAME [--site URL] [--user U] [--totp] [--host HOST].. | rm NAME |
/// revoke NAME [PROGRAM]`: `vault.toml`,
/// one of the daemon's files (DESIGN §4.15), which the daemon reads on every use. Values are
/// asked at hidden prompts, or read one per line from stdin when it is not a terminal; never
/// taken as arguments. Grants are only the owner's `/ok` or `/vault grant` in a chat. A save here
/// is not told to the agent: the owner tells it.
pub fn vault(h: &Host, mut a: Args) -> anyhow::Result<()> {
    use crate::vault::{Entry, Kind, Vault};
    let site = a.value("--site")?;
    let user = a.value("--user")?;
    let totp = a.flag("--totp");
    // The hosts a token is sent to (DESIGN §4.21): the owner's own surface, so they are taken
    // here as `/vault bind` takes them.
    let mut hosts = a.values("--host")?;
    // And the request headers it is swapped in besides Authorization.
    let headers = a.values("--header")?;
    let rest = a.rest()?;
    let usage = "usage: zephyr vault list | add login NAME --site URL [--user U] [--totp] | \
                 add token NAME [--host HOST].. [--header HEADER].. | edit NAME [--site URL] \
                 [--user U] [--totp] [--host HOST].. [--header HEADER].. | rm NAME | \
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
            if hosts.is_empty() && h.interactive {
                let asked: String = dialoguer::Input::new()
                    .with_prompt(format!(
                        "hosts {name} is sent to, e.g. api.github.com github.com (empty: bind it later)"
                    ))
                    .allow_empty(true)
                    .interact_text()?;
                hosts = asked
                    .split([' ', ','])
                    .filter(|h| !h.is_empty())
                    .map(String::from)
                    .collect();
            }
            vault.put(Entry {
                kind: Kind::Token,
                name: name.into(),
                value,
                hosts,
                headers,
                ..Entry::default()
            })?;
            let sent = match vault
                .get(name)
                .map(|e| e.hosts)
                .unwrap_or_default()
                .as_slice()
            {
                [] => format!("it is bound to no host yet: /vault bind {name} HOST.. in a chat"),
                bound => {
                    let bound: Vec<&str> = bound.iter().map(|h| crate::vault::shown(h)).collect();
                    format!("it is sent only to {}", bound.join(", "))
                }
            };
            format!("saved the token {name}; {sent}; the agent asks you before each new program uses it")
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
                Kind::Token => {
                    e.value = secret("new value, empty keeps it")?;
                    // None given keeps the ones saved.
                    e.hosts = hosts;
                    e.headers = headers;
                }
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

/// `zephyr facts list | add NAME --kind text|digits|email | rm NAME | allow NAME TARGET |
/// disallow NAME TARGET`: `facts.toml`, the owner's private facts (approvals spec §4.5), one of
/// the daemon's files, read on every use. The value is asked at a hidden prompt, or read from one
/// stdin line when it is not a terminal; never taken as an argument. Adding over an existing name
/// replaces its value and keeps its allowed targets.
pub fn facts(h: &Host, mut a: Args) -> anyhow::Result<()> {
    use crate::facts::{allow, parse, remove, render, FactKind};
    let kind = a.value("--kind")?;
    let rest = a.rest()?;
    let usage = "usage: zephyr facts list | add NAME --kind text|digits|email | rm NAME | \
                 allow NAME TARGET | disallow NAME TARGET";
    let text = match h.data.read(crate::facts::FILE) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        r => r?,
    };
    let mut facts = parse(&text)?;
    let args: Vec<&str> = rest.iter().map(String::as_str).collect();
    let said = match args[..] {
        [] | ["list"] => {
            if facts.is_empty() {
                println!("no private facts saved");
            }
            facts.iter().for_each(|f| println!("{}", f.line()));
            return Ok(());
        }
        ["add", name] => {
            let kind = kind.as_deref().unwrap_or("text");
            let kind = FactKind::parse(kind)
                .with_context(|| format!("--kind is text, digits or email\n{usage}"))?;
            let value = match h.interactive {
                true => Password::new()
                    .with_prompt(format!("value of {name} (hidden)"))
                    .allow_empty_password(true)
                    .interact()?,
                false => std::io::stdin()
                    .lines()
                    .next()
                    .transpose()?
                    .unwrap_or_default(),
            };
            println!("{}", save_fact(h, name, kind, &value)?);
            return Ok(());
        }
        ["rm", name] => match remove(&mut facts, name) {
            true => format!("removed {name}"),
            false => bail!("no fact named {name}"),
        },
        ["allow", name, target] => allow(&mut facts, name, target, true)?,
        ["disallow", name, target] => allow(&mut facts, name, target, false)?,
        _ => bail!("{usage}"),
    };
    h.data.write(crate::facts::FILE, &render(&text, &facts)?)?;
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
    let shown = h.data.load()?;
    let ask = |prompt: &str, key: &str| -> anyhow::Result<String> {
        let mut input = Input::<String>::new().with_prompt(prompt);
        if let Some(v) = conf::get(&shown, key) {
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
    let sets = if kind == "telegram" {
        println!(
            "Make a bot of zephyr's own with @BotFather in Telegram (/newbot), then send it \
             /setjoingroups and choose Disable, so no one can add it to a group."
        );
        hidden("Telegram bot token (hidden)", "ZEPHYR_TELEGRAM_TOKEN")?;
        vec![("telegram.token_env", "ZEPHYR_TELEGRAM_TOKEN".to_string())]
    } else if kind == "signal" {
        signal_add(h, &ask)?
    } else if kind == "discord" {
        let owner = ask(
            "Your Discord user id (Developer Mode, Copy User ID)",
            "owners.discord",
        )?;
        hidden("Discord bot token (hidden)", "ZEPHYR_DISCORD_TOKEN")?;
        vec![
            ("discord.token_env", "ZEPHYR_DISCORD_TOKEN".to_string()),
            ("owners.discord", list(&owner)),
        ]
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
        println!(
            "Register the BlueBubbles webhook http://{listen}/?secret=<ZEPHYR_BB_SECRET from {}/secrets.env, readable with sudo>",
            h.data.daemon.display()
        );
        vec![
            ("imessage.listen", format!("{listen:?}")),
            ("imessage.server_url", format!("{server:?}")),
            ("imessage.password_env", "ZEPHYR_BB_PASSWORD".to_string()),
            ("imessage.secret_env", "ZEPHYR_BB_SECRET".to_string()),
            ("owners.imessage", list(&owners)),
        ]
    };
    // Read again: the prompts take minutes, and the daemon may have edited it since.
    let mut doc = h.data.load()?;
    for (key, raw) in sets {
        conf::set(&mut doc, key, &raw)?;
    }
    h.data.save(&doc)?;
    if kind == "imessage" {
        imessage_forward(h)?;
    }
    Ok(())
}

/// Under the network floor the webhook is published on this host's loopback, and BlueBubbles
/// posts to the Tailscale address: `tailscale serve` joins them. Asked only when the forward is
/// missing, default yes, since the channel does not work without it. A missed detection only means
/// the question comes again; the same mapping twice is harmless.
fn imessage_forward(h: &Host) -> anyhow::Result<()> {
    let webhook = h
        .published()?
        .into_iter()
        .find(|(k, _)| *k == "imessage.listen");
    let (Some((_, port)), true) = (webhook, h.bridge()?) else {
        return Ok(());
    };
    let (port, target) = (port.to_string(), format!("tcp://127.0.0.1:{port}"));
    if super::web::forwarded(&target) {
        return Ok(());
    }
    let question = format!("Forward this machine's Tailscale port {port} to the webhook?");
    if !Confirm::new()
        .with_prompt(question)
        .default(true)
        .interact()?
    {
        println!("forward it later: sudo tailscale serve --bg --tcp {port} {target}");
        return Ok(());
    }
    let args = ["--bg", "--tcp", &port, &target];
    if let Err(e) = super::web::serve(&args, "zephyr channel add imessage") {
        println!("{e:#}");
    }
    Ok(())
}

/// `channel add signal`: registers or links zephyr's own number in the container, where root's
/// signal-cli keeps its keys (`zephyr signal-setup`), and returns the config to write.
fn signal_add(
    h: &Host,
    ask: &dyn Fn(&str, &str) -> anyhow::Result<String>,
) -> anyhow::Result<Vec<(&'static str, String)>> {
    h.need_running()?;
    if !super::quiet(h.exec(&[], &[], &["test", "-d", "/opt/zephyr/signal"])) {
        bail!("signal: not in this image (built with SIGNAL=0)");
    }
    // Every question first; the config changes only once the new number works (the caller
    // writes it), so a failure or Ctrl-C leaves the old one. The daemon's signal-cli lets the
    // account go while `signal-setup` runs (its hold), and comes back after it.
    println!(
        "Zephyr needs a Signal number of its own: a second SIM or eSIM, or a VoIP or landline \
         number. Never your own Signal account: a device linked to it receives every \
         conversation you have, and its keys speak as you to all of your contacts."
    );
    let ways = [
        "register a number zephyr will own (recommended)",
        "link to an account on a spare phone",
    ];
    let link = Select::new()
        .with_prompt("Signal")
        .items(ways)
        .default(0)
        .interact()?
        == 1;
    let number = ask(
        "Zephyr's Signal number, with its country code (+82…)",
        "signal.account",
    )?;
    let number = number.trim().to_string();
    if !number.starts_with('+') || !number[1..].bytes().all(|b| b.is_ascii_digit()) {
        bail!("{number}: want the number as +<country code><number>, digits only");
    }
    let mut argv = vec!["zephyr", "signal-setup"];
    if link {
        argv.push("link");
    } else {
        argv.extend(["register", number.as_str()]);
        if Confirm::new()
            .with_prompt("Verify by a voice call instead of SMS (a VoIP or landline number)?")
            .default(false)
            .interact()?
        {
            argv.push("--voice");
        }
    }
    run(&mut h.exec(&["-it"], &[], &argv))?;
    Ok(vec![("signal.account", format!("{number:?}"))])
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
