//! The `zephyr` binary against the scripted fake child (tests/fake-rpi.py).

use std::io::{Read, Write};
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output};
use std::sync::mpsc;
use std::time::{Duration, Instant};

const PAUSED: &str = "[zephyr] paused: messages wait in the inbox until /resume";

/// A fresh directory with `home/` (the state dir) and a `zephyr.toml` next to it.
fn setup(name: &str, extra: &str) -> (PathBuf, PathBuf) {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-cli-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    let home = dir.join("home");
    std::fs::create_dir_all(&home).unwrap();
    let fake = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fake-rpi.py");
    let cfg = dir.join("zephyr.toml");
    std::fs::write(
        &cfg,
        format!(
            "rpi = {fake:?}\nhome = {:?}\nagent_dir = {:?}\n{extra}",
            home.display(),
            dir.join("agent").display()
        ),
    )
    .unwrap();
    (home, cfg)
}

fn zephyr(cfg: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_zephyr"))
        .args(args)
        .env("ZEPHYR_CONFIG", cfg)
        .output()
        .unwrap()
}

/// A running `zephyr run`, killed on drop.
struct Daemon(Child);

impl Drop for Daemon {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn daemon(cfg: &Path, home: &Path) -> Daemon {
    let child = Command::new(env!("CARGO_BIN_EXE_zephyr"))
        .arg("run")
        .env("ZEPHYR_CONFIG", cfg)
        .spawn()
        .unwrap();
    let sock = home.join("zephyr.sock");
    let start = Instant::now();
    while !sock.exists() {
        assert!(
            start.elapsed() < Duration::from_secs(30),
            "daemon never listened"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
    Daemon(child)
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

#[test]
fn a_second_run_on_the_same_home_is_refused() {
    let (home, cfg) = setup("lock", "");
    let _first = daemon(&cfg, &home);
    let second = zephyr(&cfg, &["run"]);
    assert!(!second.status.success());
    let err = String::from_utf8_lossy(&second.stderr);
    assert!(err.contains("another zephyr is running"), "{err}");
}

#[test]
fn pause_and_resume_work_with_a_daemon_and_without_one() {
    let (home, cfg) = setup("pause", "");
    // The test runs as the owner: the chat socket sees this uid as the sender.
    let uid = std::fs::metadata(&home).unwrap().uid();
    std::fs::write(
        &cfg,
        format!(
            "{}[owners]\nchat = [\"{uid}\"]\n",
            std::fs::read_to_string(&cfg).unwrap()
        ),
    )
    .unwrap();

    let running = daemon(&cfg, &home);
    assert_eq!(stdout(&zephyr(&cfg, &["pause"])), PAUSED);
    assert!(stdout(&zephyr(&cfg, &["status"])).contains("paused: yes (user)"));
    assert_eq!(stdout(&zephyr(&cfg, &["resume"])), "[zephyr] resumed");
    assert!(stdout(&zephyr(&cfg, &["status"])).contains("paused: no"));
    let me = env!("CARGO_PKG_VERSION");
    assert!(stdout(&zephyr(&cfg, &["status"])).starts_with(&format!("version: {me}\n")));
    assert_eq!(
        stdout(&zephyr(&cfg, &["--version"])),
        format!("zephyr {me}")
    );
    drop(running);

    // With no daemon, the store carries the flag to the next start.
    assert!(zephyr(&cfg, &["pause"]).status.success());
    assert!(stdout(&zephyr(&cfg, &["status"])).contains("paused: yes (user)"));
    assert!(zephyr(&cfg, &["resume"]).status.success());
    assert!(stdout(&zephyr(&cfg, &["status"])).contains("paused: no"));
}

#[test]
fn doctor_reports_presence_and_never_values() {
    // A stand-in BlueBubbles that answers the ping and records the request line.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (seen_tx, seen) = mpsc::channel();
    std::thread::spawn(move || {
        let (mut sock, _) = listener.accept().unwrap();
        let mut buf = [0u8; 4096];
        let n = sock.read(&mut buf).unwrap();
        let line = String::from_utf8_lossy(&buf[..n])
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        let _ = seen_tx.send(line);
        let _ =
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
    });

    // mode 0644, so doctor must report it
    let token = std::env::temp_dir().join(format!("zephyr-cli-token-{}", std::process::id()));
    std::fs::write(&token, "not-a-real-token").unwrap();
    std::fs::set_permissions(&token, std::fs::Permissions::from_mode(0o644)).unwrap();
    let (_, cfg) = setup(
        "doctor",
        &format!(
            r#"[discord]
token_env = "ZEPHYR_TEST_DISCORD_TOKEN_UNSET"
[imessage]
listen = "127.0.0.1:0"
server_url = "http://127.0.0.1:{port}"
password_env = "ZEPHYR_TEST_BB_PASSWORD"
secret_env = "ZEPHYR_TEST_BB_SECRET"
[attach]
port = 0
token_file = "{}"
"#,
            token.display()
        ),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_zephyr"))
        .arg("doctor")
        .env("ZEPHYR_CONFIG", &cfg)
        .env_remove("ZEPHYR_TEST_DISCORD_TOKEN_UNSET")
        .env("ZEPHYR_TEST_BB_PASSWORD", "pw-sekret-123")
        .env("ZEPHYR_TEST_BB_SECRET", "sec-sekret-456")
        .output()
        .unwrap();
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));

    // Every rpi command zephyr sends is one the fake child (like rpi 0.18) says it supports.
    assert!(
        text.contains("ok   ready.supportedCommands covers"),
        "{text}"
    );
    assert!(
        text.contains("FAIL env ZEPHYR_TEST_DISCORD_TOKEN_UNSET is set (discord token)"),
        "{text}"
    );
    assert!(
        text.contains("ok   env ZEPHYR_TEST_BB_PASSWORD is set (imessage password)"),
        "{text}"
    );
    assert!(
        text.contains("ok   env ZEPHYR_TEST_BB_SECRET is set (imessage webhook secret)"),
        "{text}"
    );
    assert!(
        text.contains("ok   BlueBubbles answers GET /api/v1/ping"),
        "{text}"
    );
    assert!(
        text.contains("FAIL") && text.contains("is not readable by group or others"),
        "{text}"
    );
    assert!(
        !text.contains("pw-sekret-123") && !text.contains("sec-sekret-456"),
        "a secret was printed"
    );
    let request = seen
        .recv_timeout(Duration::from_secs(10))
        .expect("no ping was sent");
    assert!(
        request.starts_with("GET /api/v1/ping?password="),
        "{request}"
    );
}

#[test]
fn doctor_warns_when_typing_needs_the_private_api() {
    // A stand-in BlueBubbles: the ping answers 200, and server info reports the Private API off.
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for mut sock in listener.incoming().flatten() {
            let mut buf = [0u8; 4096];
            let n = sock.read(&mut buf).unwrap_or(0);
            let line = String::from_utf8_lossy(&buf[..n]).to_string();
            let body = if line.starts_with("GET /api/v1/server/info") {
                r#"{"status":200,"message":"Success","data":{"private_api":false}}"#
            } else {
                ""
            };
            let _ = sock.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                )
                .as_bytes(),
            );
        }
    });

    let (_, cfg) = setup(
        "doctor-typing",
        &format!(
            r#"[imessage]
listen = "127.0.0.1:0"
server_url = "http://127.0.0.1:{port}"
password_env = "ZEPHYR_TEST_BB_PASSWORD"
secret_env = "ZEPHYR_TEST_BB_SECRET"
typing = true
"#
        ),
    );
    let out = Command::new(env!("CARGO_BIN_EXE_zephyr"))
        .arg("doctor")
        .env("ZEPHYR_CONFIG", &cfg)
        .env("ZEPHYR_TEST_BB_PASSWORD", "pw-sekret-123")
        .env("ZEPHYR_TEST_BB_SECRET", "sec-sekret-456")
        .output()
        .unwrap();
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));

    assert!(
        text.contains(
            "warn iMessage typing needs the BlueBubbles Private API with its helper connected; enable it on the Mac or set [imessage] typing = false"
        ),
        "{text}"
    );
    assert!(
        !text.contains("pw-sekret-123") && !text.contains("sec-sekret-456"),
        "a secret was printed"
    );
}

#[test]
fn doctor_warns_when_agent_dir_has_extensions() {
    let warning = "rpi children ignore it (--local-only); providers come from the image";
    let (_, cfg) = setup("doctor-ext", "");
    let agent = cfg.parent().unwrap().join("agent");
    let run = || stdout(&zephyr(&cfg, &["doctor"]));
    let text = run();
    assert!(!text.contains(warning));
    // The image's CLI tools are checked whether or not this machine has them.
    assert!(
        ["ok   gh works", "warn gh does not run"]
            .iter()
            .any(|l| text.contains(l)),
        "{text}"
    );
    std::fs::create_dir_all(agent.join("extensions")).unwrap();
    assert!(!run().contains(warning), "an empty extensions dir is fine");
    std::fs::write(agent.join("extensions/librpi_providers.so"), "").unwrap();
    assert!(run().contains(warning));
}

#[test]
fn doctor_warns_when_an_extension_path_is_missing() {
    let (_, cfg) = setup("doctor-missing-ext", "");
    let dir = cfg.parent().unwrap();
    let here = dir.join("libzephyr_policy.so");
    std::fs::write(&here, "").unwrap();
    let gone = dir.join("librpi_mcp_adapter.so");
    let mut text = std::fs::read_to_string(&cfg).unwrap();
    text.push_str(&format!(
        "policy_plugin = {here:?}\nextensions = [{gone:?}]\n"
    ));
    std::fs::write(&cfg, text).unwrap();

    let text = stdout(&zephyr(&cfg, &["doctor"]));
    assert!(
        text.contains(&format!(
            "warn {} does not exist: no child can load it",
            gone.display()
        )),
        "{text}"
    );
    assert!(
        !text.contains(&format!("warn {}", here.display())),
        "{text}"
    );
}

#[test]
fn status_lists_each_pooled_account() {
    let (home, cfg) = setup(
        "status-pool",
        "[[account]]\nname = 'a'\nmodel = 'anthropic/m'\n[[account]]\nname = 'b'\nmodel = 'openai-codex/m'\n",
    );
    std::fs::create_dir_all(home.join("accounts/a")).unwrap();
    std::fs::write(
        home.join("accounts/a/auth.json"),
        r#"{"anthropic": {"type": "oauth", "access": "SECRET"}}"#,
    )
    .unwrap();
    let out = zephyr(&cfg, &["status"]);
    assert!(out.status.success(), "{out:?}");
    let text = stdout(&out);
    assert!(
        text.contains("account a (anthropic/m): ready; today 0 tokens"),
        "{text}"
    );
    assert!(
        text.contains("account b (openai-codex/m): needs login"),
        "{text}"
    );
    assert!(!text.contains("SECRET"));
}

#[test]
fn secrets_env_in_home_is_loaded_without_overriding_the_environment() {
    let (home, cfg) = setup(
        "dotenv",
        "[discord]\ntoken_env = \"ZEPHYR_CLI_DOTENV_TOKEN\"\n",
    );
    let label = "env ZEPHYR_CLI_DOTENV_TOKEN is set";
    assert!(stdout(&zephyr(&cfg, &["doctor"])).contains(&format!("FAIL {label}")));
    std::fs::write(
        home.join("secrets.env"),
        "# x\nZEPHYR_CLI_DOTENV_TOKEN=\"tok-sekret\"\n",
    )
    .unwrap();
    let out = zephyr(&cfg, &["doctor"]);
    assert!(
        stdout(&out).contains(&format!("ok   {label}")),
        "{}",
        stdout(&out)
    );
    assert!(!stdout(&out).contains("tok-sekret"));
    let out = Command::new(env!("CARGO_BIN_EXE_zephyr"))
        .arg("doctor")
        .env("ZEPHYR_CONFIG", &cfg)
        .env("ZEPHYR_CLI_DOTENV_TOKEN", "")
        .output()
        .unwrap();
    assert!(
        stdout(&out).contains(&format!("FAIL {label}")),
        "a set variable wins"
    );
}

#[test]
fn doctor_probes_as_the_first_pooled_account() {
    let (home, cfg) = setup(
        "doctor-pool",
        "[[account]]\nname = 'a'\nmodel = 'anthropic/m'\n[[account]]\nname = 'b'\nmodel = 'openai-codex/m'\n",
    );
    let first = home.join("accounts/a");
    std::fs::create_dir_all(&first).unwrap();
    zephyr(&cfg, &["doctor"]);
    let starts = std::fs::read_to_string(first.join("starts.log")).unwrap_or_default();
    assert_eq!(starts, "anthropic/m\n");
}

/// The environment of the rpi child the daemon started (fake-rpi writes `<home>/pid`).
fn child_env(home: &Path) -> String {
    let pid = std::fs::read_to_string(home.join("pid")).unwrap_or_default();
    String::from_utf8_lossy(&std::fs::read(format!("/proc/{pid}/environ")).unwrap_or_default())
        .replace('\0', "\n")
}

/// Polls `done` until it holds. The deadline is generous: a whole-suite run (many daemons and
/// fake children at once) can take far longer than one test alone, and waiting costs nothing.
fn wait_for(what: &str, mut done: impl FnMut() -> bool) {
    let start = Instant::now();
    while !done() {
        assert!(start.elapsed() < Duration::from_secs(30), "{what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn file_reads_and_replaces_only_the_daemons_files() {
    let (_, cfg) = setup("file", "");
    let dir = cfg.parent().unwrap();
    let put = |name: &str, text: &str| {
        let mut child = Command::new(env!("CARGO_BIN_EXE_zephyr"))
            .args(["__file", "put", name])
            .env("ZEPHYR_CONFIG", &cfg)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(text.as_bytes())
            .unwrap();
        child.wait().unwrap()
    };
    assert!(put("secrets.env", "A=1\n").success());
    assert_eq!(
        std::fs::read_to_string(dir.join("secrets.env")).unwrap(),
        "A=1\n"
    );
    let mode = std::fs::metadata(dir.join("secrets.env")).unwrap().mode() & 0o777;
    assert_eq!(mode, 0o600);
    assert_eq!(
        stdout(&zephyr(&cfg, &["__file", "get", "secrets.env"])),
        "A=1"
    );
    assert_eq!(
        zephyr(&cfg, &["__file", "get", "host.toml"]).status.code(),
        Some(3)
    );
    // A zephyr.toml this daemon would refuse is not written.
    let before = std::fs::read_to_string(&cfg).unwrap();
    assert!(!put("zephyr.toml", &format!("{before}\nnot_a_key = 1\n")).success());
    assert_eq!(std::fs::read_to_string(&cfg).unwrap(), before);
    assert!(put("zephyr.toml", &format!("{before}\n# kept\n")).success());
    // Nothing outside the list, and no paths.
    assert!(!put("state.db", "x").success());
    assert!(!zephyr(&cfg, &["__file", "get", "../zephyr.toml"])
        .status
        .success());
}

#[test]
fn the_agent_sees_no_secret_unless_agent_env_passes_it() {
    let (home, cfg) = setup("hidden", "[agent]\nenv = [\"ZEPHYR_CLI_PASSED\"]\n");
    std::fs::write(
        home.join("secrets.env"),
        "ZEPHYR_CLI_HIDDEN=one\nZEPHYR_CLI_PASSED=two\n",
    )
    .unwrap();
    let running = daemon(&cfg, &home);
    wait_for("the child starts", || child_env(&home).contains("PATH="));
    let env = child_env(&home);
    assert!(
        !env.contains("ZEPHYR_CLI_HIDDEN"),
        "a secret reached the agent"
    );
    assert!(env.contains("ZEPHYR_CLI_PASSED=two"));
    // The daemon holds every secret; the agent's uid cannot read its environment.
    let err = std::fs::read(format!("/proc/{}/environ", running.0.id())).unwrap_err();
    assert_eq!(err.kind(), std::io::ErrorKind::PermissionDenied);
}

#[test]
fn a_changed_secret_reexecs_the_daemon_in_place() {
    // `[agent] env` lets this one through, so the child shows which value the daemon read.
    let (home, cfg) = setup("reexec", "[agent]\nenv = [\"ZEPHYR_CLI_RELOAD\"]\n");
    let uid = std::fs::metadata(&home).unwrap().uid();
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("{text}[owners]\nchat = [\"{uid}\"]\n")).unwrap();
    std::fs::write(home.join("secrets.env"), "ZEPHYR_CLI_RELOAD=one\n").unwrap();
    let mut running = daemon(&cfg, &home);
    wait_for("the child has the secret", || {
        child_env(&home).contains("ZEPHYR_CLI_RELOAD=one")
    });

    std::fs::write(home.join("secrets.env"), "ZEPHYR_CLI_RELOAD=two\n").unwrap();
    // The daemon's 5 s tick may see the edit first, so only the outcome is checked here;
    // tests/router.rs checks the reply text.
    zephyr(&cfg, &["reload"]);
    // The same process, exec'd anew, reads the new value and starts a new child with it.
    wait_for("no re-exec", || {
        child_env(&home).contains("ZEPHYR_CLI_RELOAD=two")
    });
    assert!(
        running.0.try_wait().unwrap().is_none(),
        "the daemon process lives on"
    );
    wait_for("the socket serves again", || {
        stdout(&zephyr(&cfg, &["pause"])) == PAUSED
    });
    let second = zephyr(&cfg, &["run"]);
    assert!(String::from_utf8_lossy(&second.stderr).contains("another zephyr is running"));
    drop(running);
}

#[test]
fn a_background_command_outlives_a_reexec_and_reports() {
    let (home, cfg) = setup("reexec-proc", "[agent]\nenv = [\"ZEPHYR_CLI_RELOAD\"]\n");
    let uid = std::fs::metadata(&home).unwrap().uid();
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("{text}[owners]\nchat = [\"{uid}\"]\n")).unwrap();
    std::fs::write(home.join("secrets.env"), "ZEPHYR_CLI_RELOAD=one\n").unwrap();
    let running = daemon(&cfg, &home);
    wait_for("the child has the secret", || {
        child_env(&home).contains("ZEPHYR_CLI_RELOAD=one")
    });
    let mut chat = std::os::unix::net::UnixStream::connect(home.join("zephyr.sock")).unwrap();
    writeln!(
        chat,
        r#"{{"id": "p-1", "text": "[call exec {{\"command\": \"sleep 4; echo survived\", \"background\": true}}] go"}}"#
    )
    .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(&chat));
    let moved = lines.find(|l| {
        l.as_ref()
            .map_or(true, |l| l.contains("Moved to the background"))
    });
    assert!(
        moved.is_some_and(|l| l.is_ok()),
        "the command was not backgrounded"
    );

    std::fs::write(home.join("secrets.env"), "ZEPHYR_CLI_RELOAD=two\n").unwrap();
    stdout(&zephyr(&cfg, &["reload"]));
    wait_for("the re-exec", || {
        child_env(&home).contains("ZEPHYR_CLI_RELOAD=two")
    });
    // The old process's socket closed on exec; a reply sent meanwhile waits for this one.
    let chat = std::os::unix::net::UnixStream::connect(home.join("zephyr.sock")).unwrap();
    chat.set_read_timeout(Some(Duration::from_secs(20)))
        .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(&chat));
    let report = lines.find(|l| {
        l.as_ref()
            .map_or(true, |l| l.contains("process #1 exited 0"))
    });
    assert!(
        report.is_some_and(|l| l.is_ok_and(|l| l.contains("survived"))),
        "the adopted command did not report after the re-exec"
    );
    drop(running);
}

#[test]
fn a_reexec_waits_for_the_running_reply() {
    let (home, cfg) = setup("reexec-busy", "[agent]\nenv = [\"ZEPHYR_CLI_RELOAD\"]\n");
    let uid = std::fs::metadata(&home).unwrap().uid();
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("{text}[owners]\nchat = [\"{uid}\"]\n")).unwrap();
    std::fs::write(home.join("secrets.env"), "ZEPHYR_CLI_RELOAD=one\n").unwrap();
    std::fs::write(home.join("slow"), "3").unwrap();
    let running = daemon(&cfg, &home);
    wait_for("the child has the secret", || {
        child_env(&home).contains("ZEPHYR_CLI_RELOAD=one")
    });
    let mut chat = std::os::unix::net::UnixStream::connect(home.join("zephyr.sock")).unwrap();
    chat.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    writeln!(chat, r#"{{"id": "busy-1", "text": "say: finished"}}"#).unwrap();
    let prompts = || {
        std::fs::read_to_string(home.join("log.jsonl"))
            .unwrap_or_default()
            .matches(r#""type":"prompt""#)
            .count()
    };
    wait_for("the run started", || prompts() == 1);

    std::fs::write(home.join("secrets.env"), "ZEPHYR_CLI_RELOAD=two\n").unwrap();
    assert_eq!(
        stdout(&zephyr(&cfg, &["reload"])),
        "[zephyr] restarting zephyr's channels after the current reply"
    );
    // Still taking messages: one sent now is answered before the re-exec too.
    writeln!(chat, r#"{{"id": "busy-2", "text": "and this"}}"#).unwrap();
    // Both replies come on this connection, so the old process sent them before it exec'd.
    // (The `reload` answer comes here too: chat writes to every connection of the uid.)
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(&chat));
    for want in ["finished", "and this"] {
        let reply = lines.find(|l| l.as_ref().map_or(true, |l| l.contains(want)));
        assert!(
            reply.is_some_and(|l| l.is_ok()),
            "no reply to {want:?} before the re-exec"
        );
    }
    wait_for("no re-exec", || {
        child_env(&home).contains("ZEPHYR_CLI_RELOAD=two")
    });
    assert_eq!(prompts(), 2, "each message went to rpi once");
    drop(running);
}

/// `zephyr supervise`, as the image runs it; stopped on drop with SIGTERM, which it passes on.
struct Supervisor(Child);

impl Drop for Supervisor {
    fn drop(&mut self) {
        // SAFETY: a plain syscall to our own child.
        unsafe { libc::kill(self.0.id() as i32, libc::SIGTERM) };
        let start = Instant::now();
        while start.elapsed() < Duration::from_secs(20) {
            if let Ok(Some(_)) = self.0.try_wait() {
                return;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// The processes `pid` started that are still its children.
fn children(pid: u32) -> Vec<i32> {
    std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children"))
        .unwrap_or_default()
        .split_whitespace()
        .filter_map(|p| p.parse().ok())
        .collect()
}

#[test]
fn a_background_command_outlives_a_daemon_crash_under_supervise() {
    let (home, cfg) = setup("supervise", "");
    let uid = std::fs::metadata(&home).unwrap().uid();
    let text = std::fs::read_to_string(&cfg).unwrap();
    std::fs::write(&cfg, format!("{text}[owners]\nchat = [\"{uid}\"]\n")).unwrap();
    let mut sup = Supervisor(
        Command::new(env!("CARGO_BIN_EXE_zephyr"))
            .arg("supervise")
            .env("ZEPHYR_CONFIG", &cfg)
            .spawn()
            .unwrap(),
    );
    let sup_pid = sup.0.id();
    let daemon_pid = || children(sup_pid).first().copied();
    wait_for("the daemon came up", || {
        daemon_pid().is_some() && home.join("zephyr.sock").exists()
    });
    let first = daemon_pid().unwrap();
    let mut chat = std::os::unix::net::UnixStream::connect(home.join("zephyr.sock")).unwrap();
    writeln!(
        chat,
        r#"{{"id": "p-1", "text": "[call exec {{\"command\": \"sleep 8; echo survived-crash\", \"background\": true}}] go"}}"#
    )
    .unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(&chat));
    let moved = lines.find(|l| {
        l.as_ref()
            .map_or(true, |l| l.contains("Moved to the background"))
    });
    assert!(
        moved.is_some_and(|l| l.is_ok()),
        "the command was not backgrounded"
    );

    // SAFETY: a plain syscall to the daemon this test's supervisor started.
    unsafe { libc::kill(first, libc::SIGKILL) };
    wait_for("the supervisor started the daemon again", || {
        daemon_pid().is_some_and(|p| p != first)
    });
    assert!(
        sup.0.try_wait().unwrap().is_none(),
        "the supervisor itself stays up"
    );
    let start = Instant::now();
    let chat = loop {
        // The new daemon listens once it is up; until then the old socket refuses.
        match std::os::unix::net::UnixStream::connect(home.join("zephyr.sock")) {
            Ok(c) => break c,
            Err(_) if start.elapsed() < Duration::from_secs(30) => {
                std::thread::sleep(Duration::from_millis(100))
            }
            Err(e) => panic!("the new daemon never listened: {e}"),
        }
    };
    chat.set_read_timeout(Some(Duration::from_secs(30)))
        .unwrap();
    writeln!(&chat, r#"{{"id": "p-2", "text": "/process list"}}"#).unwrap();
    let mut lines = std::io::BufRead::lines(std::io::BufReader::new(&chat));
    let listed = lines.find(|l| l.as_ref().map_or(true, |l| l.contains("processes:")));
    assert!(
        listed.is_some_and(|l| l.is_ok_and(|l| l.contains("#1 running"))),
        "the command was not adopted after the crash"
    );
    // Not the new daemon's child: its exit code comes from the wrapper's record.
    let report = lines.find(|l| l.as_ref().map_or(true, |l| l.contains("process #1 exited")));
    let report = report.and_then(|l| l.ok()).unwrap_or_default();
    assert!(
        report.contains("process #1 exited 0") && report.contains("survived-crash"),
        "{report}"
    );
}
