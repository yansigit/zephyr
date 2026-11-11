//! `zephyr web setup | enrol` on the host (DESIGN §4.3.3, §4.8): put the web app behind
//! `tailscale serve` and enrol the first passkey. Every change to the host is asked for first.

use super::{conf, run, Args, Host};
use anyhow::{bail, Context};
use std::process::Command;

/// The web app's port in the container, on the host's loopback.
const PORT: &str = "7793";

pub(super) fn main(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let yes = a.flag("--yes");
    let rest = a.rest()?;
    match rest.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["setup"] => setup(h, yes),
        ["enrol"] if !yes => enrol(h, None),
        ["enrol", name] if !yes => enrol(h, Some(name)),
        _ => bail!("usage: zephyr web setup [--yes] | web enrol [NAME]"),
    }
}

/// `docker exec … zephyr web enrol [NAME]`: the daemon's one-time code, from the chat socket.
fn enrol(h: &Host, name: Option<&str>) -> anyhow::Result<()> {
    h.need_running()?;
    let argv = ["zephyr", "web", "enrol"].into_iter().chain(name);
    run(&mut h.exec(&[], &[], &argv.collect::<Vec<_>>()))
}

/// Asks before a change. `--yes` answers yes; without it and without a terminal, nothing is
/// changed.
fn ask(h: &Host, yes: bool, question: &str) -> anyhow::Result<bool> {
    if yes {
        println!("{question} yes");
        return Ok(true);
    }
    if !h.interactive {
        bail!("zephyr web setup asks before each change: run it on a terminal, or pass --yes");
    }
    Ok(dialoguer::Confirm::new()
        .with_prompt(question)
        .default(true)
        .interact()?)
}

/// This machine's tailnet name, once Tailscale runs and gives it an HTTPS certificate.
fn tailnet_name() -> anyhow::Result<String> {
    let how = "install Tailscale (https://tailscale.com/download) and run `tailscale up`";
    let out = Command::new("tailscale")
        .args(["status", "--json"])
        .output();
    let status: serde_json::Value = match out {
        Ok(out) if out.status.success() => serde_json::from_slice(&out.stdout).unwrap_or_default(),
        _ => bail!("tailscale is not running here; {how}"),
    };
    if status["BackendState"] != "Running" {
        bail!("tailscale is not logged in; {how}");
    }
    let name = status["Self"]["DNSName"].as_str().unwrap_or_default();
    let name = name.trim_end_matches('.').to_lowercase();
    if name.is_empty() {
        bail!("tailscale gave this machine no name; {how}");
    }
    let certs = status["CertDomains"].as_array();
    if !certs.is_some_and(|all| all.iter().any(|c| c.as_str() == Some(&name))) {
        bail!(
            "turn on HTTPS certificates in the Tailscale admin console (DNS → HTTPS Certificates), \
             then run zephyr web setup again"
        );
    }
    Ok(name)
}

/// The daemon's `zephyr set KEY VALUE`, over the chat socket: its checks, its change log.
fn set(h: &Host, key: &str, value: &str) -> anyhow::Result<()> {
    run(&mut h.exec(&[], &[], &["zephyr", "set", key, value]))
}

fn setup(h: &Host, yes: bool) -> anyhow::Result<()> {
    // Under host networking the agent could reach the port; the daemon would not start the app.
    if !h.bridge()? {
        bail!("{}", crate::web::NEEDS_FLOOR);
    }
    // 1, 2: the name, and HTTPS on it. Nothing is changed before both are known.
    let name = tailnet_name()?;
    println!("this machine is {name} on your tailnet, with HTTPS certificates on");

    // 3: zephyr.toml.
    // A port the owner chose is kept. Under the network floor `zephyr up` publishes it on this
    // machine's loopback (`egress::published`), which is where `tailscale serve` forwards to;
    // under host networking the daemon is on that loopback itself.
    let mut doc = h.data.load()?;
    let port = conf::get(&doc, "web.port").unwrap_or_else(|| PORT.into());
    let (port, target) = (port.as_str(), format!("http://127.0.0.1:{port}"));
    let same = conf::get(&doc, "web.host").as_deref() == Some(name.as_str())
        && conf::get(&doc, "web.port").as_deref() == Some(port);
    if !same {
        if !ask(
            h,
            yes,
            &format!("Write [web] host = \"{name}\", port = {port} to zephyr.toml?"),
        )? {
            return Ok(());
        }
        conf::set(&mut doc, "web.host", &name)?;
        conf::set(&mut doc, "web.port", port)?;
        toml::from_str::<crate::config::Config>(&doc.to_string())
            .map_err(anyhow::Error::from)
            .and_then(|c| c.validate())
            .context("[web] would make the config invalid")?;
        h.data.save(&doc)?;
    }

    // 4: tailscale serve, tailnet only.
    if !ask(
        h,
        yes,
        &format!("Run `tailscale serve --bg {target}` (HTTPS on your tailnet only)?"),
    )? {
        return Ok(());
    }
    let serve = Command::new("tailscale")
        .args(["serve", "--bg", &target])
        .output()
        .context("running tailscale serve")?;
    if !serve.status.success() {
        eprint!("{}", String::from_utf8_lossy(&serve.stderr));
        bail!(
            "tailscale serve was refused. Run it once as root, or let this user manage serve:\n  \
             sudo tailscale serve --bg {target}\n  sudo tailscale set --operator=$USER\n\
             then run zephyr web setup again"
        );
    }
    // 5
    println!(
        "never point tailscale funnel at this port: the web app refuses funnel traffic, and a \
         funnel would put it on the internet"
    );

    // 6: the daemon reads [web] at start.
    if !ask(
        h,
        yes,
        "Start zephyr again so the web app listens (`zephyr up`)?",
    )? {
        return Ok(());
    }
    h.ensure_image(false, false)?;
    h.up_checked()?;

    // 7: a list the owner wrote keeps what it has; passkey is offered.
    let strong = doc.get("settings").and_then(|s| s.get("strong_channels"));
    let strong: Option<Vec<&str>> = strong
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect());
    if let Some(list) = strong.filter(|list| !list.contains(&"passkey")) {
        let with = format!("{},passkey", list.join(","));
        let question = "Let an approval signed with a passkey loosen protected settings \
                        (add passkey to settings.strong_channels)?";
        if ask(h, yes, question)? {
            set(h, "settings.strong_channels", &with)?;
        }
    }

    // 8: the first passkey.
    println!("add this phone or laptop: open the link below on it (it works once, for 10 minutes)");
    enrol(h, None)?;

    // 9: once a passkey exists, a payment can wait for it.
    if !yes {
        println!("press Enter once the device is added (or now, to finish)");
        std::io::stdin().read_line(&mut String::new())?;
    }
    let listed = h
        .exec(&[], &[], &["zephyr", "web"])
        .output()
        .context("running docker exec")?;
    let listed = String::from_utf8_lossy(&listed.stdout).into_owned();
    print!("{listed}");
    if listed.contains("passkeys: ") && !listed.contains("passkeys: none") {
        let question =
            "Make payments wait for a passkey or the terminal, never Discord or iMessage \
                        (approvals.strong_only = loosen,money)?";
        if ask(h, yes, question)? {
            set(h, "approvals.strong_only", "loosen,money")?;
        }
    }
    println!("the web app is at https://{name}");
    Ok(())
}

/// The host's part of `zephyr status`: whether `tailscale serve` forwards to the web app. Nothing
/// when `[web]` is not set.
pub(super) fn status(h: &Host) {
    let Ok(doc) = h.data.load() else {
        return;
    };
    let Some(host) = conf::get(&doc, "web.host") else {
        return;
    };
    if !h.bridge().unwrap_or(true) {
        return println!("web: {}", crate::web::NEEDS_FLOOR);
    }
    let port = conf::get(&doc, "web.port").unwrap_or_else(|| PORT.into());
    let serve = Command::new("tailscale").args(["serve", "status"]).output();
    let serve = serve
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let target = format!("http://127.0.0.1:{port}");
    // The port as a whole word: 7793 is not forwarded by a line for 77930.
    let forwards = serve.split_whitespace().any(|word| word == target);
    match forwards {
        true => println!("web: tailscale serve forwards https://{host}"),
        false => println!(
            "web: tailscale serve does not forward to 127.0.0.1:{port}; run zephyr web setup"
        ),
    }
}
