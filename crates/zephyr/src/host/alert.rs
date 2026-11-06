//! `zephyr alert setup|check|test|off`: a systemd user timer on the host checks the gateway every
//! 5 minutes and posts to an ntfy topic when it goes down and when it comes back.

use super::{agent_ids, home, run, Args, Host};
use anyhow::{bail, Context};
use std::path::PathBuf;
use std::process::{Command, Stdio};

const DEFAULT_SERVER: &str = "https://ntfy.sh";
const USAGE: &str = "usage: zephyr alert setup TOPIC [--server URL] | check | test | off";

pub fn main(h: &Host, mut a: Args) -> anyhow::Result<()> {
    let server = a.value("--server")?;
    let rest = a.rest()?;
    let rest: Vec<&str> = rest.iter().map(String::as_str).collect();
    match (&rest[..], server) {
        (["setup", topic], server) => setup(topic, server.as_deref().unwrap_or(DEFAULT_SERVER)),
        (["check"], None) => check(h),
        (["test"], None) => {
            let url = target()?;
            post(&url, "zephyr alert test", "a test alert from zephyr")?;
            println!("sent a test alert to {url}");
            Ok(())
        }
        (["off"], None) => off(),
        _ => bail!("{USAGE}"),
    }
}

fn units_dir() -> PathBuf {
    home().join(".config/systemd/user")
}

/// `up` or `down`, then the failed-reply count: what the last check found, so only a change is
/// posted.
fn state_path() -> PathBuf {
    home().join(".local/state/zephyr/alert-state")
}

/// `<server>/<topic>`, in the owner's own home: a check must post even when Docker itself is down,
/// so it cannot read its target through the container.
fn url_path() -> PathBuf {
    home().join(".config/zephyr/alert-url")
}

fn systemctl(args: &[&str]) -> Command {
    let mut cmd = Command::new("systemctl");
    cmd.arg("--user").args(args);
    cmd
}

fn setup(topic: &str, server: &str) -> anyhow::Result<()> {
    // Both end up in a URL: nothing that could change its path or query.
    let topic_ok = (1..=64).contains(&topic.len())
        && topic
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    if !topic_ok {
        bail!("an ntfy topic is 1 to 64 letters, digits, - or _");
    }
    let server = server.trim_end_matches('/');
    let scheme = server.starts_with("https://") || server.starts_with("http://");
    if !scheme || server.contains(|c: char| c.is_whitespace() || c == '?' || c == '#') {
        bail!("--server takes an http(s) URL, e.g. {DEFAULT_SERVER}");
    }
    let url_file = url_path();
    std::fs::create_dir_all(url_file.parent().context("no parent")?)?;
    std::fs::write(&url_file, format!("{server}/{topic}\n"))?;
    let exe = std::env::current_exe().context("finding this zephyr")?;
    // A check against another container (`ZEPHYR_CONTAINER`) keeps checking that one.
    let env: String = ["ZEPHYR_CONTAINER", "ZEPHYR_IMAGE"]
        .iter()
        .filter_map(|k| Some(format!("Environment={k}={}\n", std::env::var(k).ok()?)))
        .collect();
    let service = format!(
        "[Unit]\nDescription=zephyr outage alert (zephyr alert setup)\n\n\
         [Service]\nType=oneshot\nTimeoutStartSec=4min\nExecStart={} alert check\n{env}",
        exe.display()
    );
    let timer = "[Unit]\nDescription=Check zephyr every 5 minutes\n\n\
                 [Timer]\nOnCalendar=*:0/5\n\n[Install]\nWantedBy=timers.target\n";
    let dir = units_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join("zephyr-alert.service"), service)?;
    std::fs::write(dir.join("zephyr-alert.timer"), timer)?;
    run(&mut systemctl(&["daemon-reload"]))?;
    run(&mut systemctl(&["enable", "--now", "zephyr-alert.timer"]))?;
    println!("zephyr alert check runs every 5 minutes and posts to {server}/{topic}");
    let uid = agent_ids().0.to_string();
    let linger = Command::new("loginctl")
        .args(["show-user", &uid, "-p", "Linger"])
        .stderr(Stdio::null())
        .output();
    if !linger.is_ok_and(|o| String::from_utf8_lossy(&o.stdout).trim() == "Linger=yes") {
        println!(
            "hint: run `loginctl enable-linger $USER` so the timer runs while you are logged out"
        );
    }
    Ok(())
}

/// `<server>/<topic>`, as `setup` saved it.
fn target() -> anyhow::Result<String> {
    let url = std::fs::read_to_string(url_path())
        .context("no alert is set up: zephyr alert setup TOPIC")?;
    Ok(url.trim().to_string())
}

/// Posts when the gateway's health changed since the last check, and when more replies failed to
/// send than before (a count, not a state: failed rows stay until they are pruned). The state is
/// saved only once the posts went out, so a failed post is tried again at the next check.
fn check(h: &Host) -> anyhow::Result<()> {
    let url = target()?;
    let (problem, failed) = problem(h);
    let path = state_path();
    let saved = std::fs::read_to_string(&path).unwrap_or_default();
    let mut words = saved.split_whitespace();
    let was_down = words.next() == Some("down");
    let was_failed: i64 = words.next().and_then(|n| n.parse().ok()).unwrap_or(0);
    match (&problem, was_down) {
        (Some(why), false) => post(
            &url,
            "zephyr is down",
            &format!("zephyr is unhealthy: {why}"),
        )?,
        (None, true) => post(&url, "zephyr is back", "zephyr is back")?,
        _ => {}
    }
    if failed > was_failed {
        let body = format!(
            "{} more replies failed to send ({failed} in all)",
            failed - was_failed
        );
        post(&url, "zephyr could not deliver replies", &body)?;
    }
    std::fs::create_dir_all(path.parent().context("no parent")?)?;
    let state = if problem.is_some() { "down" } else { "up" };
    // While the status cannot be read, the last count stands.
    let failed = if problem.is_some() {
        was_failed.max(failed)
    } else {
        failed
    };
    std::fs::write(&path, format!("{state} {failed}\n"))?;
    match problem {
        Some(why) => println!("unhealthy: {why}"),
        None => println!("healthy"),
    }
    Ok(())
}

/// What is wrong with the gateway, if anything (its container is not running, or `zephyr status`
/// fails), and how many replies have failed to send.
fn problem(h: &Host) -> (Option<String>, i64) {
    if !h.running() {
        let state = h.state().unwrap_or_else(|| "not created".into());
        return (Some(format!("container {} is {state}", h.container)), 0);
    }
    let out = match h.exec(&[], &[], &["zephyr", "status"]).output() {
        Ok(out) => out,
        Err(e) => return (Some(format!("running docker exec failed: {e}")), 0),
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let last = err.lines().last().unwrap_or_default().trim();
        return (
            Some(format!("zephyr status failed ({}) {last}", out.status)),
            0,
        );
    }
    let failed: i64 = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| match l.split_whitespace().collect::<Vec<_>>()[..] {
            ["outbox", "failed", n] => n.parse::<i64>().ok(),
            _ => None,
        })
        .sum();
    (None, failed)
}

fn post(url: &str, title: &str, body: &str) -> anyhow::Result<()> {
    let status = Command::new("curl")
        .args([
            "-fsS",
            "-m",
            "30",
            "-H",
            &format!("Title: {title}"),
            "-d",
            body,
            url,
        ])
        .stdout(Stdio::null())
        .status()
        .context("running curl")?;
    if !status.success() {
        bail!("posting to {url} failed ({status})");
    }
    Ok(())
}

fn off() -> anyhow::Result<()> {
    // Not set up is fine: there is then nothing to disable.
    let _ = systemctl(&["disable", "--now", "zephyr-alert.timer"]).status();
    for unit in ["zephyr-alert.timer", "zephyr-alert.service"] {
        match std::fs::remove_file(units_dir().join(unit)) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    run(&mut systemctl(&["daemon-reload"]))?;
    let _ = std::fs::remove_file(state_path());
    let _ = std::fs::remove_file(url_path());
    println!("outage alerts are off");
    Ok(())
}
