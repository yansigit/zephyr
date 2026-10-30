//! Host commands against a fake `docker` (and `gh`) on PATH: the real Docker is never called.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONTAINER: &str = "zephyr-lanetest";
const GH_TOKEN: &str = "gho_FAKE_GH_TOKEN_0123";
const CLAUDE_TOKEN: &str = "sk-ant-oat01-FAKE-CLAUDE-TOKEN";

/// Records each call's argv (fields split by \x1f) in calls.log, and stdin of `exec -i` in stdin.log.
const DOCKER: &str = r#"#!/bin/bash
log="$SHIM_DIR/calls.log"
(IFS=$'\x1f'; echo "docker$IFS$*") >> "$log"
[ -n "$GH_TOKEN" ] && echo "GH_TOKEN set" >> "$SHIM_DIR/env.log"
case "$1" in
  inspect) echo "${SHIM_STATE:-running}"; exit 0 ;;
  image) exit "${SHIM_IMAGE_MISSING:-0}" ;;
  exec)
    [ "$2" = "-i" ] && cat >> "$SHIM_DIR/stdin.log"
    case " $* " in
      *" --list-models "*) printf 'provider model context\nanthropic claude-sonnet-5 200K\nopenai-codex gpt-5.5 400K\nopenai-codex gpt-5.4 400K\n' ;;
      *" auth check "*) echo '{"ready":true,"provider":"claude-code","source":"auth.json"}' ;;
    esac ;;
esac
exit 0
"#;

const GH: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "gh$IFS$*") >> "$SHIM_DIR/calls.log"
case "$1 $2" in
  "auth token") echo "gho_FAKE_GH_TOKEN_0123" ;;
  "api user") echo "tester" ;;
esac
"#;

struct Env {
    dir: PathBuf,
    home: PathBuf,
    projects: PathBuf,
}

impl Env {
    fn new(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!(
            "zephyr-host-{name}-{}-{}",
            std::process::id(),
            zephyr::unix_millis()
        ));
        let (home, projects, bin) = (dir.join("home"), dir.join("projects"), dir.join("bin"));
        for d in [&home, &projects.join("app/node_modules/x"), &bin] {
            std::fs::create_dir_all(d).unwrap();
        }
        std::fs::write(projects.join("app/.env"), "SECRET=1").unwrap();
        std::fs::write(projects.join("app/node_modules/x/.env"), "").unwrap();
        for (name, body) in [("docker", DOCKER), ("gh", GH)] {
            std::fs::write(bin.join(name), body).unwrap();
            std::fs::set_permissions(bin.join(name), std::fs::Permissions::from_mode(0o755))
                .unwrap();
        }
        Env {
            dir,
            home,
            projects,
        }
    }

    fn zephyr(&self, args: &[&str]) -> Output {
        self.zephyr_env(args, &[])
    }

    fn zephyr_env(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let out = Command::new(env!("CARGO_BIN_EXE_zephyr"))
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("SHIM_DIR", &self.dir)
            .env("ZEPHYR_CONTAINER", CONTAINER)
            .env("ZEPHYR_IMAGE", "zephyr-lanetest")
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.dir.join("output.log"))
            .and_then(|mut f| std::io::Write::write_all(&mut f, text.as_bytes()))
            .unwrap();
        out
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.zephyr(args);
        assert!(
            out.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout(&out)
    }

    /// Calls recorded since the last take, each as its argv.
    fn calls(&self) -> Vec<Vec<String>> {
        let path = self.dir.join("calls.log");
        let text = std::fs::read_to_string(&path).unwrap_or_default();
        let _ = std::fs::remove_file(&path);
        text.lines()
            .map(|l| l.split('\x1f').map(String::from).collect())
            .collect()
    }

    fn data(&self) -> PathBuf {
        self.home.join(".zephyr")
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.data().join("zephyr.toml")).unwrap()
    }

    fn init(&self, extra: &[&str]) {
        let projects = self.projects.display().to_string();
        let mut args = vec!["init", "--non-interactive", "--projects", &projects];
        args.extend(extra);
        self.ok(&args);
    }
}

fn stdout(out: &Output) -> String {
    String::from_utf8_lossy(&out.stdout).into_owned()
}

fn find<'a>(calls: &'a [Vec<String>], prefix: &[&str]) -> &'a Vec<String> {
    calls
        .iter()
        .find(|c| c.len() >= prefix.len() && c[..prefix.len()] == *prefix)
        .unwrap_or_else(|| panic!("no call {prefix:?} in {calls:#?}"))
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o777
}

#[test]
fn init_non_interactive_builds_the_layout_and_starts_the_container() {
    let env = Env::new("init");
    env.init(&[
        "--account",
        "codex-a=openai-codex/gpt-5.5",
        "--provider",
        "anthropic",
        "--model",
        "anthropic/claude-sonnet-5",
    ]);
    let data = env.data();
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&data.join("secrets.env")), 0o600);
    assert_eq!(mode(&data.join("tui.token")), 0o600);
    assert!(data.join("accounts").is_dir() && data.join(".rpi/agent").is_dir());
    let projects = env.projects.canonicalize().unwrap().display().to_string();
    assert!(std::fs::read_to_string(data.join("host.toml"))
        .unwrap()
        .contains(&projects));

    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(cfg["model"].as_str(), Some("anthropic/claude-sonnet-5"));
    let accounts: Vec<(&str, &str)> = cfg["account"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["name"].as_str().unwrap(), a["model"].as_str().unwrap()))
        .collect();
    assert_eq!(
        accounts,
        [
            ("codex-a", "openai-codex/gpt-5.5"),
            ("anthropic-1", "anthropic/claude-sonnet-5")
        ]
    );
    assert_eq!(cfg["extensions"].as_array().unwrap().len(), 3);

    let calls = env.calls();
    find(&calls, &["docker", "version"]);
    find(&calls, &["docker", "image", "inspect", "zephyr-lanetest"]);
    find(&calls, &["docker", "rm", "-f", CONTAINER]);
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    let uid = std::fs::metadata(&env.home).unwrap().uid();
    assert!(
        run.iter().any(|a| a.starts_with(&format!("{uid}:"))),
        "{run:?}"
    );
    assert!(run.contains(&format!("type=bind,src={},dst=/zephyr", data.display())));
    assert!(run.contains(&format!("type=bind,src={projects},dst={projects}")));
    assert!(run.contains(&format!(
        "type=bind,src=/dev/null,dst={projects}/app/.env,readonly"
    )));
    assert!(!run.iter().any(|a| a.contains("node_modules")), "{run:?}");
    assert!(!run.iter().any(|a| a.contains("env-file")), "{run:?}");
    assert_eq!(run.last().unwrap(), "zephyr-lanetest");
    find(&calls, &["docker", "exec", CONTAINER, "zephyr", "doctor"]);

    // Re-running keeps what is there, comments included.
    let path = data.join("zephyr.toml");
    std::fs::write(&path, format!("# mine\n{}", env.config())).unwrap();
    let token = std::fs::read_to_string(data.join("tui.token")).unwrap();
    env.init(&["--no-start", "--provider", "anthropic"]);
    assert!(env.config().starts_with("# mine\n"));
    assert_eq!(
        std::fs::read_to_string(data.join("tui.token")).unwrap(),
        token
    );
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(cfg["account"].as_array().unwrap().len(), 2);
    assert!(env.calls().is_empty(), "--no-start calls nothing");

    let out = env.zephyr(&[
        "init",
        "--non-interactive",
        "--no-start",
        "--provider",
        "cursor",
    ]);
    assert!(
        !out.status.success(),
        "a provider without a model is refused"
    );
}

#[test]
fn up_build_passes_the_gh_token_as_a_secret_only() {
    let env = Env::new("build");
    env.init(&["--no-start"]);
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../..");
    let out = env.zephyr_env(&["up", "--build"], &[("ZEPHYR_SRC", src)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = env.calls();
    let build = find(&calls, &["docker", "build", "-t", "zephyr-lanetest"]);
    assert!(
        build
            .windows(2)
            .any(|w| w == ["--secret", "id=gh_token,env=GH_TOKEN"]),
        "{build:?}"
    );
    assert_eq!(build.last().unwrap(), src);
    find(&calls, &["docker", "run"]);
    assert!(std::fs::read_to_string(env.dir.join("env.log"))
        .unwrap()
        .contains("GH_TOKEN set"));
    assert!(!format!("{calls:?}").contains(GH_TOKEN));

    // A missing image is pulled; a failed pull logs in with gh's token on stdin.
    let out = env.zephyr_env(&["up"], &[("SHIM_IMAGE_MISSING", "1")]);
    assert!(out.status.success());
    let calls = env.calls();
    find(&calls, &["docker", "pull", "zephyr-lanetest"]);
    assert!(
        !calls.iter().any(|c| c[1] == "build"),
        "pull succeeded, so no build"
    );
}

#[test]
fn login_feeds_secrets_on_stdin_and_records_the_account() {
    let env = Env::new("login");
    env.init(&["--no-start"]);
    let out = env.zephyr_env(
        &[
            "login",
            "claude-code",
            "--account",
            "claude-main",
            "--model",
            "claude-code/claude-sonnet-5",
        ],
        &[("CLAUDE_CODE_OAUTH_TOKEN", CLAUDE_TOKEN)],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("ok   test call through claude-main"),
        "{}",
        stdout(&out)
    );
    let calls = env.calls();
    let acct = "RPI_CODING_AGENT_DIR=/zephyr/accounts/claude-main";
    let login = find(
        &calls,
        &[
            "docker", "exec", "-i", "-e", acct, CONTAINER, "script", "-qec",
        ],
    );
    assert!(
        login[8].starts_with("rpi auth login --provider claude-code --extension /opt/zephyr/ext/"),
        "{login:?}"
    );
    find(
        &calls,
        &[
            "docker", "exec", "-e", acct, CONTAINER, "rpi", "auth", "check",
        ],
    );
    let verify = find(
        &calls,
        &["docker", "exec", "-e", acct, CONTAINER, "rpi", "-p"],
    );
    assert!(verify
        .windows(2)
        .any(|w| w == ["--model", "claude-code/claude-sonnet-5"]));
    assert_eq!(
        std::fs::read_to_string(env.dir.join("stdin.log")).unwrap(),
        format!("{CLAUDE_TOKEN}\n")
    );
    assert!(
        !format!("{calls:?}").contains(CLAUDE_TOKEN),
        "token in argv"
    );
    assert!(!std::fs::read_to_string(env.dir.join("output.log"))
        .unwrap()
        .contains(CLAUDE_TOKEN));
    assert_eq!(mode(&env.data().join("accounts/claude-main")), 0o700);

    // OAuth providers run rpi's own login on a terminal; with no --model the first listed one is used.
    env.ok(&["login", "openai-codex", "--no-verify"]);
    let calls = env.calls();
    let login = find(
        &calls,
        &[
            "docker",
            "exec",
            "-it",
            "-e",
            "RPI_CODING_AGENT_DIR=/zephyr/accounts/openai-codex-1",
            CONTAINER,
            "rpi",
            "auth",
            "login",
            "--provider",
            "openai-codex",
            "--extension",
        ],
    );
    assert!(login.contains(&"/opt/zephyr/ext/rpi-claude-agent/librpi_claude_agent.so".to_string()));
    assert!(
        !calls.iter().any(|c| c.contains(&"-p".to_string())),
        "--no-verify"
    );
    let names = env.ok(&["accounts"]);
    assert!(
        names.contains("claude-main") && names.contains("openai-codex-1"),
        "{names}"
    );

    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    let models: Vec<&str> = cfg["account"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| a["model"].as_str().unwrap())
        .collect();
    assert_eq!(
        models,
        ["claude-code/claude-sonnet-5", "openai-codex/gpt-5.5"]
    );

    assert!(!env.zephyr(&["login", "nope"]).status.success());
    assert!(!env
        .zephyr_env(&["login", "anthropic"], &[("SHIM_STATE", "exited")])
        .status
        .success());
}

#[test]
fn accounts_model_and_config_edit_the_toml() {
    let env = Env::new("edit");
    env.init(&[
        "--no-start",
        "--account",
        "a=anthropic/claude-sonnet-5",
        "--account",
        "b=openai-codex/gpt-5.5",
    ]);
    std::fs::create_dir_all(env.data().join("accounts/a")).unwrap();
    env.ok(&["accounts", "order", "b", "a"]);
    assert!(!env.zephyr(&["accounts", "order", "b"]).status.success());
    let cfg = env.config();
    assert!(
        cfg.find("name = \"b\"").unwrap() < cfg.find("name = \"a\"").unwrap(),
        "{cfg}"
    );
    // Several accounts: `model` needs --account and a model of that account's provider.
    assert!(!env
        .zephyr(&["model", "openai-codex/gpt-5.4"])
        .status
        .success());
    assert!(!env
        .zephyr(&["model", "--account", "a", "openai-codex/gpt-5.4"])
        .status
        .success());
    env.ok(&["model", "--account", "b", "openai-codex/gpt-5.4"]);
    assert!(env.config().contains("model = \"openai-codex/gpt-5.4\""));

    env.ok(&["accounts", "remove", "a"]);
    assert!(!env.data().join("accounts/a").exists());
    assert!(!env.config().contains("name = \"a\""));

    // One account left: `model` sets it and the default.
    env.ok(&["model", "openai-codex/gpt-5.5"]);
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(cfg["model"].as_str(), Some("openai-codex/gpt-5.5"));
    assert_eq!(
        cfg["account"][0]["model"].as_str(),
        Some("openai-codex/gpt-5.5")
    );
    // The picker offers only the account's provider: the first codex model, not the first listed.
    env.ok(&["model", "openai-codex/gpt-5.4"]);
    env.ok(&["model"]);
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(
        cfg["account"][0]["model"].as_str(),
        Some("openai-codex/gpt-5.5")
    );
    find(
        &env.calls(),
        &[
            "docker",
            "exec",
            CONTAINER,
            "rpi",
            "--list-models",
            "--extension",
        ],
    );
}

#[test]
fn config_get_and_set_check_the_daemon_schema() {
    let env = Env::new("config");
    env.init(&["--no-start"]);
    assert_eq!(env.ok(&["config", "get", "attach.port"]).trim(), "7790");
    env.ok(&["config", "set", "budget.daily_tokens", "5000"]);
    assert_eq!(
        env.ok(&["config", "get", "budget.daily_tokens"]).trim(),
        "5000"
    );
    env.ok(&["config", "set", "timezone", "Asia/Seoul"]);
    assert_eq!(env.ok(&["config", "get", "timezone"]).trim(), "Asia/Seoul");
    let before = env.config();
    assert!(!env.zephyr(&["config", "set", "typo", "1"]).status.success());
    assert_eq!(env.config(), before, "a refused set writes nothing");
    assert!(
        before.contains("# `zephyr model` picks one"),
        "comments kept"
    );
}

#[test]
fn lifecycle_commands_build_the_expected_docker_calls() {
    let env = Env::new("life");
    env.init(&["--no-start"]);
    let cases: [(&[&str], &[&str]); 7] = [
        (&["down"], &["docker", "stop", CONTAINER]),
        (&["restart"], &["docker", "restart", CONTAINER]),
        (&["logs", "-f"], &["docker", "logs", "-f", CONTAINER]),
        (
            &["status"],
            &["docker", "exec", CONTAINER, "zephyr", "status"],
        ),
        (
            &["chat"],
            &["docker", "exec", "-it", CONTAINER, "zephyr", "chat"],
        ),
        (
            &["pause"],
            &["docker", "exec", CONTAINER, "zephyr", "pause"],
        ),
        (&["upgrade"], &["docker", "pull", "zephyr-lanetest"]),
    ];
    for (args, want) in cases {
        env.ok(args);
        find(&env.calls(), want);
    }
    assert!(env
        .ok(&["status"])
        .contains(&format!("container {CONTAINER}: running")));

    let token = std::fs::read_to_string(env.data().join("tui.token")).unwrap();
    env.ok(&["attach"]);
    let calls = env.calls();
    find(
        &calls,
        &[
            "docker",
            "exec",
            "-it",
            "-e",
            "RPI_SERVER_TOKEN",
            CONTAINER,
            "rpi",
            "--connect",
            "127.0.0.1:7790",
        ],
    );
    assert!(!format!("{calls:?}").contains(token.trim()));

    assert!(
        !env.zephyr(&["uninstall"]).status.success(),
        "no terminal, no --yes: refused"
    );
    assert!(env.data().exists());
    env.ok(&["uninstall", "--keep-data"]);
    let calls = env.calls();
    find(&calls, &["docker", "rm", "-f", CONTAINER]);
    find(&calls, &["docker", "rmi", "zephyr-lanetest"]);
    assert!(env.data().exists());
    env.ok(&["uninstall", "--yes"]);
    assert!(!env.data().exists());
}

#[test]
fn doctor_fix_repairs_modes() {
    let env = Env::new("doctor");
    env.init(&["--no-start"]);
    let secrets = env.data().join("secrets.env");
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = env.zephyr(&["doctor"]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains(&format!("FAIL {} is private", secrets.display())));
    env.ok(&["doctor", "--fix"]);
    assert_eq!(mode(&secrets), 0o600);
}
