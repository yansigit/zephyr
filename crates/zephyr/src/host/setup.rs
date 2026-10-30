//! `init`, `login`, `accounts`, `model` and `channel`: the steps that ask questions.

use super::{conf, feed, home, output, run, Args, Host, USAGE};
use anyhow::{bail, Context};
use dialoguer::{Confirm, Input, MultiSelect, Password, Select, Sort};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::Stdio;

/// Provider ids as `rpi auth login --provider` takes them, with menu labels.
pub const PROVIDERS: [(&str, &str); 6] = [
    ("opencode-go", "OpenCode Go"),
    ("antigravity", "Antigravity"),
    ("openai-codex", "ChatGPT/Codex"),
    ("claude-code", "Claude (Agent SDK)"),
    ("cursor", "Cursor (chat only)"),
    ("anthropic", "Anthropic API key"),
];

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
    conf::add_image_extensions(&mut doc);
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
        .map(|(id, _)| have.iter().any(|p| p == id))
        .collect();
    let labels: Vec<&str> = PROVIDERS.iter().map(|(_, l)| *l).collect();
    let chosen = MultiSelect::new()
        .with_prompt("Providers (space to select, enter to confirm)")
        .items(&labels)
        .defaults(&defaults)
        .interact()?;
    for i in chosen {
        let id = PROVIDERS[i].0;
        let mut add = !have.iter().any(|p| p == id);
        loop {
            if add {
                if let Err(e) = login(h, id, None, None, true) {
                    println!("{id}: {e:#}");
                }
            }
            add = Confirm::new()
                .with_prompt(format!("Add another {} account?", PROVIDERS[i].1))
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
    if !PROVIDERS.iter().any(|(p, _)| *p == id) {
        let ids: Vec<_> = PROVIDERS.iter().map(|(p, _)| *p).collect();
        bail!("unknown provider {id}: one of {}", ids.join(", "));
    }
    Ok(())
}

/// `<provider>-<n>` with the smallest free n.
fn next_name(doc: &toml_edit::DocumentMut, provider: &str) -> String {
    let taken: Vec<String> = conf::accounts(doc).into_iter().map(|(n, _)| n).collect();
    (1..)
        .map(|n| format!("{provider}-{n}"))
        .find(|n| !taken.contains(n))
        .expect("a free name")
}

pub fn login_cmd(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let account = a.value("--account")?;
    let model = a.value("--model")?;
    let verify = !a.flag("--no-verify");
    match &a.rest()?[..] {
        [provider] => login(h, provider, account, model, verify),
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
            println!("Sign in with your Claude subscription. Copy the token it prints at the end.");
            run(&mut h.exec(&["-it"], &[], &[CLAUDE_CLI, "setup-token"]))?;
            Some(
                Password::new()
                    .with_prompt("Paste the token (hidden)")
                    .interact()?,
            )
        }
        None => None,
    };
    if let Some(token) = token {
        // rpi reads keys from a terminal, so `script` gives it one. The pty echoes the key back,
        // so the output is dropped; `auth check` tells whether it was stored.
        let login = rpi(&exts, &["auth", "login", "--provider", provider]).join(" ");
        feed(
            &mut h.exec(&["-i"], &env, &["script", "-qec", &login, "/dev/null"]),
            token.trim(),
        )?;
        let check = output(&mut h.exec(
            &[],
            &env,
            &rpi(&exts, &["auth", "check", "--json", "--provider", provider]),
        ))?;
        if !check.contains("\"ready\":true") {
            bail!("the token was not stored for {name}");
        }
    } else {
        run(&mut h.exec(
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
        let out = h
            .exec(
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
    let text = output(&mut h.exec(&[], &[], &argv))?;
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
    println!("saved; `zephyr restart` to apply");
    Ok(())
}

/// Sets the default model, and the only account's model when there is exactly one.
pub fn model(h: &Host, a: Args) -> anyhow::Result<()> {
    let rest = a.rest()?;
    let mut doc = h.data.load()?;
    let model = match &rest[..] {
        [m] if m.contains('/') => m.clone(),
        [] => {
            h.need_running()?;
            let exts = conf::extension_args(&doc);
            let exts: Vec<&str> = exts.iter().map(String::as_str).collect();
            pick_model(h, &exts, None)?
        }
        _ => bail!("usage: zephyr model [PROVIDER/MODEL]"),
    };
    conf::set(&mut doc, "model", &model)?;
    if let [(name, _)] = &conf::accounts(&doc)[..] {
        conf::upsert_account(&mut doc, &name.clone(), &model);
    }
    h.data.save(&doc)?;
    println!("model {model}; `zephyr restart` to apply");
    Ok(())
}

pub fn channel(h: &Host, a: Args) -> anyhow::Result<()> {
    match &a.rest()?[..] {
        [add, kind] if add == "add" && (kind == "discord" || kind == "imessage") => {
            channel_add(h, kind)?;
            run(&mut super::docker(&["restart", &h.container]))?;
            run(&mut h.exec(&[], &[], &["zephyr", "doctor"]))
        }
        _ => bail!("usage: zephyr channel add discord|imessage"),
    }
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
    let secrets = h.data.secrets();
    let hidden = |prompt: &str, env: &str| -> anyhow::Result<()> {
        let v = Password::new().with_prompt(prompt).interact()?;
        conf::set_secret(&secrets, env, v.trim())
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
        conf::set_secret(&secrets, "ZEPHYR_BB_SECRET", &conf::random_hex(24)?)?;
        conf::set(&mut doc, "imessage.listen", &format!("{listen:?}"))?;
        conf::set(&mut doc, "imessage.server_url", &format!("{server:?}"))?;
        conf::set(&mut doc, "imessage.password_env", "ZEPHYR_BB_PASSWORD")?;
        conf::set(&mut doc, "imessage.secret_env", "ZEPHYR_BB_SECRET")?;
        conf::set(&mut doc, "owners.imessage", &list(&owners))?;
        println!(
            "Register the BlueBubbles webhook http://{listen}/?secret=<ZEPHYR_BB_SECRET from {}>",
            secrets.display()
        );
    }
    h.data.save(&doc)
}
