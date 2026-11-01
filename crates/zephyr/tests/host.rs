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
# `zephyr __file` in the container serves the daemon's files from $SHIM_DIR/files.
case " $* " in
  *" __file get "*) f="$SHIM_DIR/files/${@: -1}"; [ -f "$f" ] || exit 3; cat "$f"; exit 0 ;;
  *" __file put "*) [ -n "$SHIM_PUT_FAIL" ] && exit 1; mkdir -p "$SHIM_DIR/files"; cat > "$SHIM_DIR/files/${@: -1}"; exit 0 ;;
esac
[ -n "$GH_TOKEN" ] && echo "GH_TOKEN set" >> "$SHIM_DIR/env.log"
case "$1" in
  inspect) echo "${SHIM_STATE:-running}"; exit 0 ;;
  info) echo "${SHIM_RUNTIMES:-io.containerd.runc.v2 runc}"; exit 0 ;;
  image) exit "${SHIM_IMAGE_MISSING:-0}" ;;
  exec)
    case " $* " in *" -i "*) cat >> "$SHIM_DIR/stdin.log" ;; esac
    case " $* " in
      *" --list-models "*) printf 'provider model context\nanthropic claude-sonnet-5 200K\nopenai-codex gpt-5.5 400K\nopenai-codex gpt-5.4 400K\n' ;;
      *" auth check "*) echo '{"ready":true,"provider":"claude-code","source":"auth.json"}' ;;
      *" zephyr status "*) echo "daemon: ${SHIM_DAEMON:-running}" ;;
      *" zephyr tui "*) exit "${SHIM_TUI_EXIT:-0}" ;;
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

    /// `zephyr <args>` with a terminal as stdin and stdout, through script(1). Its output is
    /// what the terminal received.
    fn on_a_tty(&self, args: &str, env: &[(&str, &str)]) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        Command::new("script")
            .args([
                "-qec",
                &format!("{} {args}", env!("CARGO_BIN_EXE_zephyr")),
                "/dev/null",
            ])
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("SHIM_DIR", &self.dir)
            .env("ZEPHYR_CONTAINER", CONTAINER)
            .env("ZEPHYR_IMAGE", "zephyr-lanetest")
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap()
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

    /// The daemon's own files (DESIGN §4.4). Root's in real use; this user's here, since the
    /// shim's chown does nothing.
    fn daemon(&self) -> PathBuf {
        self.home.join(".zephyr-daemon")
    }

    fn config(&self) -> String {
        std::fs::read_to_string(self.daemon().join("zephyr.toml")).unwrap()
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

/// This user's `uid:gid`, the agent's in the container: the owner of a file it just made.
fn ids() -> String {
    let probe = std::env::temp_dir().join(format!("zephyr-host-ids-{}", std::process::id()));
    std::fs::write(&probe, "").unwrap();
    let me = std::fs::metadata(&probe).unwrap();
    let _ = std::fs::remove_file(&probe);
    format!("{}:{}", me.uid(), me.gid())
}

/// A `docker exec` as the agent (this user, in its home) whose argv starts with `rest`.
fn find_agent<'a>(calls: &'a [Vec<String>], rest: &[&str]) -> &'a Vec<String> {
    let ids = ids();
    let prefix: Vec<&str> = ["docker", "exec", "-u", &ids, "-w", "/zephyr"]
        .into_iter()
        .chain(rest.iter().copied())
        .collect();
    find(calls, &prefix)
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
    let (data, daemon) = (env.data(), env.daemon());
    assert_eq!(mode(&data), 0o700);
    assert_eq!(mode(&daemon), 0o700);
    assert_eq!(mode(&daemon.join("secrets.env")), 0o600);
    assert_eq!(mode(&daemon.join("tui.token")), 0o600);
    assert!(
        !data.join("zephyr.toml").exists(),
        "the agent's home holds no daemon file"
    );
    assert!(data.join("accounts").is_dir() && data.join(".rpi/agent").is_dir());
    let projects = env.projects.canonicalize().unwrap().display().to_string();
    assert!(std::fs::read_to_string(daemon.join("host.toml"))
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
    assert_eq!(cfg["extensions"].as_array().unwrap().len(), 4);

    let calls = env.calls();
    find(&calls, &["docker", "version"]);
    find(&calls, &["docker", "image", "inspect", "zephyr-lanetest"]);
    find(&calls, &["docker", "rm", "-f", CONTAINER]);
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    let uid = std::fs::metadata(&env.home).unwrap().uid();
    // Root without file overrides runs the daemon; the agent gets this user's ids.
    assert!(run.windows(2).any(|w| w == ["--user", "0:0"]), "{run:?}");
    assert!(run.contains(&format!("ZEPHYR_AGENT_UID={uid}")), "{run:?}");
    assert!(run.contains(&format!("type=bind,src={},dst=/zephyr", data.display())));
    assert!(run.contains(&format!(
        "type=bind,src={},dst=/var/lib/zephyr",
        daemon.display()
    )));
    assert!(run.contains(&"ZEPHYR_CONFIG=/var/lib/zephyr/zephyr.toml".to_string()));
    assert!(run.contains(&format!("type=bind,src={projects},dst={projects}")));
    assert!(run.contains(&format!(
        "type=bind,src=/dev/null,dst={projects}/app/.env,readonly"
    )));
    assert!(!run.iter().any(|a| a.contains("node_modules")), "{run:?}");
    assert!(!run.iter().any(|a| a.contains("env-file")), "{run:?}");
    for pair in [
        ["--cap-drop", "ALL"],
        ["--cap-add", "SETUID"],
        ["--cap-add", "SETGID"],
        ["--cap-add", "KILL"],
        ["--security-opt", "no-new-privileges"],
        ["--pids-limit", "4096"],
        ["-w", "/"],
    ] {
        assert!(run.windows(2).any(|w| w == pair), "{pair:?} in {run:?}");
    }
    assert!(
        !run.iter().any(|a| a == "--memory"),
        "no memory cap unless set"
    );
    assert!(
        !run.iter().any(|a| a == "--gpus"),
        "no GPU without its runtime"
    );
    assert_eq!(run.last().unwrap(), "zephyr-lanetest");
    find(&calls, &["docker", "exec", CONTAINER, "zephyr", "doctor"]);

    // With the NVIDIA runtime the GPU goes in, for the browser's WebGL.
    let out = env.zephyr_env(&["up"], &[("SHIM_RUNTIMES", "nvidia runc")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(run.windows(2).any(|w| w == ["--gpus", "all"]), "{run:?}");
    assert!(run.contains(&"NVIDIA_DRIVER_CAPABILITIES=graphics,utility".to_string()));

    // Re-running keeps what is there, comments included.
    let path = daemon.join("zephyr.toml");
    std::fs::write(&path, format!("# mine\n{}", env.config())).unwrap();
    let token = std::fs::read_to_string(daemon.join("tui.token")).unwrap();
    env.init(&["--no-start", "--provider", "anthropic"]);
    assert!(env.config().starts_with("# mine\n"));
    assert_eq!(
        std::fs::read_to_string(daemon.join("tui.token")).unwrap(),
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
    let text = stdout(&out);
    assert!(text.contains("pulling zephyr-lanetest"), "{text}");
    // `up` returns once the daemon answers, not when the container merely exists.
    assert!(text.contains(&format!("{CONTAINER} is ready")), "{text}");
    let calls = env.calls();
    find(&calls, &["docker", "pull", "zephyr-lanetest"]);
    find(&calls, &["docker", "exec", CONTAINER, "zephyr", "status"]);
    assert!(
        !calls.iter().any(|c| c[1] == "build"),
        "pull succeeded, so no build"
    );
}

#[test]
fn up_caps_memory_only_when_host_toml_sets_it() {
    let env = Env::new("memory");
    env.init(&["--no-start"]);
    let host = env.daemon().join("host.toml");
    let text = std::fs::read_to_string(&host).unwrap();
    std::fs::write(&host, format!("{text}memory = \"6g\"\n")).unwrap();
    let out = env.zephyr(&["up"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(run.windows(2).any(|w| w == ["--memory", "6g"]), "{run:?}");
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
    let login = find_agent(&calls, &["-i", "-e", acct, CONTAINER, "script", "-qec"]);
    assert!(
        login[12].starts_with("rpi auth login --provider claude-code --extension /opt/zephyr/ext/"),
        "{login:?}"
    );
    find_agent(&calls, &["-e", acct, CONTAINER, "rpi", "auth", "check"]);
    let verify = find_agent(&calls, &["-e", acct, CONTAINER, "rpi", "-p"]);
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
    let login = find_agent(
        &calls,
        &[
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
    env.calls();
    env.ok(&["model", "--account", "b", "openai-codex/gpt-5.4"]);
    assert!(env.config().contains("model = \"openai-codex/gpt-5.4\""));
    find(
        &env.calls(),
        &["docker", "exec", CONTAINER, "zephyr", "reload"],
    );

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
    find_agent(
        &env.calls(),
        &[CONTAINER, "rpi", "--list-models", "--extension"],
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
    // A running daemon reloads; a stopped one picks the file up when it starts.
    env.calls();
    env.ok(&["config", "set", "budget.daily_tokens", "6000"]);
    find(
        &env.calls(),
        &["docker", "exec", CONTAINER, "zephyr", "reload"],
    );
    let out = env.zephyr_env(
        &["config", "set", "budget.daily_tokens", "7000"],
        &[("SHIM_STATE", "exited")],
    );
    assert!(
        stdout(&out).contains("applies when zephyr starts"),
        "{out:?}"
    );
    assert!(!env
        .calls()
        .iter()
        .any(|c| c.contains(&"reload".to_string())));
    let before = env.config();
    assert!(!env
        .zephyr(&["config", "set", "timezone", "Mars/Olympus"])
        .status
        .success());
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
        // Not a terminal (stdin is null here): line mode, without a tty.
        (
            &["chat"],
            &["docker", "exec", "-i", CONTAINER, "zephyr", "chat"],
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

    let token = std::fs::read_to_string(env.daemon().join("tui.token")).unwrap();
    env.ok(&["attach"]);
    let calls = env.calls();
    let tui = find(&calls, &["docker", "exec", "-it"]);
    assert!(
        tui.ends_with(&[CONTAINER.into(), "zephyr".into(), "tui".into()]),
        "{tui:?}"
    );
    // The token goes by name only; the display variables follow, so the theme sees the terminal.
    for var in ["RPI_SERVER_TOKEN", "TERM", "COLORFGBG", "NO_COLOR", "LANG"] {
        assert!(tui.windows(2).any(|w| w == ["-e", var]), "{var} in {tui:?}");
    }
    // The container runs on UTC, so the host's zone goes along for the TUI's times.
    let tz = env.zephyr_env(&["attach"], &[("TZ", "Europe/Berlin")]);
    assert!(tz.status.success());
    let calls = env.calls();
    let tui = find(&calls, &["docker", "exec", "-it"]);
    assert!(
        tui.windows(2).any(|w| w == ["-e", "TZ=Europe/Berlin"]),
        "{tui:?}"
    );
    assert!(!format!("{calls:?}").contains(token.trim()));

    env.ok(&["attach", "--stock"]);
    let calls = env.calls();
    find(
        &calls,
        &[
            "docker",
            "exec",
            "-it",
            "-e",
            "RPI_SERVER_TOKEN",
            "-e",
            "HOME=/tmp",
            "-e",
            "RPI_CODING_AGENT_DIR=/tmp/zephyr-attach",
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
    assert!(!env.daemon().exists());
}

/// On a terminal `chat` opens the TUI, which needs the attach port; without `[attach]` it keeps
/// the line client.
#[test]
fn chat_on_a_terminal_falls_back_to_the_line_client_without_attach() {
    if Command::new("script").arg("--version").output().is_err() {
        return eprintln!("skipped: no script(1) to give zephyr a terminal");
    }
    let env = Env::new("chat-tty");
    env.init(&["--no-start"]);
    let chat_on_a_tty = || {
        let out = env.on_a_tty("chat", &[]);
        assert!(out.status.success(), "{}", stdout(&out));
        env.calls()
    };
    let calls = chat_on_a_tty();
    let tui = find(&calls, &["docker", "exec", "-it"]);
    assert!(tui.ends_with(&["zephyr".into(), "tui".into()]), "{tui:?}");

    let cfg = env.daemon().join("zephyr.toml");
    let text = std::fs::read_to_string(&cfg).unwrap();
    let start = text.find("[attach]").unwrap();
    let end = text[start + 1..]
        .find("\n[")
        .map_or(text.len(), |i| start + 1 + i);
    std::fs::write(&cfg, format!("{}{}", &text[..start], &text[end..])).unwrap();
    find(
        &chat_on_a_tty(),
        &["docker", "exec", "-it", CONTAINER, "zephyr", "chat"],
    );
}

/// A `zephyr tui` killed with its container cannot undo its terminal modes; the host does.
#[test]
fn a_killed_tui_leaves_the_terminal_usable() {
    if Command::new("script").arg("--version").output().is_err() {
        return eprintln!("skipped: no script(1) to give zephyr a terminal");
    }
    let env = Env::new("tui-killed");
    env.init(&["--no-start"]);
    let out = env.on_a_tty("attach", &[("SHIM_TUI_EXIT", "137")]);
    let seen = stdout(&out);
    assert!(!out.status.success());
    for mode in ["\x1b[?1049l", "\x1b[?1000l", "\x1b[?2004l", "\x1b[?25h"] {
        assert!(seen.contains(mode), "{mode:?} not reset: {seen:?}");
    }
    assert!(seen.contains("zephyr attach"), "{seen:?}");
    // A clean detach leaves the terminal to the TUI, which restored it itself.
    let out = env.on_a_tty("attach", &[]);
    assert!(out.status.success());
    assert!(!stdout(&out).contains("\x1b[?1049l"));
}

#[test]
fn up_moves_an_old_layout_out_of_the_agents_home_once() {
    let env = Env::new("migrate");
    env.init(&["--no-start"]);
    // The layout before the split: every file in ~/.zephyr, the chat owner this uid.
    let (data, daemon) = (env.data(), env.daemon());
    let uid = std::fs::metadata(&env.home).unwrap().uid();
    let old = env
        .config()
        .replace(
            "[owners]\nchat = [\"0\"]",
            &format!("[owners]\nchat = [\"{uid}\"]"),
        )
        .replace("/var/lib/zephyr/tui.token", "/zephyr/tui.token")
        .replace("state = \"/var/lib/zephyr\"\n", "");
    for name in ["secrets.env", "tui.token", "host.toml"] {
        std::fs::rename(daemon.join(name), data.join(name)).unwrap();
    }
    std::fs::remove_file(daemon.join("zephyr.toml")).unwrap();
    std::fs::remove_dir(&daemon).unwrap();
    std::fs::write(data.join("zephyr.toml"), &old).unwrap();
    std::fs::write(data.join("state.db"), "db").unwrap();
    std::fs::write(data.join("zephyr.lock"), "").unwrap();

    env.ok(&["up"]);
    for name in [
        "zephyr.toml",
        "secrets.env",
        "tui.token",
        "host.toml",
        "state.db",
    ] {
        assert!(
            daemon.join(name).exists() && !data.join(name).exists(),
            "{name}"
        );
    }
    assert!(!data.join("zephyr.lock").exists());
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(cfg["state"].as_str(), Some("/var/lib/zephyr"));
    assert_eq!(
        cfg["attach"]["token_file"].as_str(),
        Some("/var/lib/zephyr/tui.token")
    );
    assert_eq!(
        cfg["owners"]["chat"].as_array().unwrap(),
        &[toml::Value::from("0")]
    );
    let calls = env.calls();
    let chown = find(&calls, &["docker", "run", "--rm", "--user", "0"]);
    assert_eq!(chown.last().unwrap(), "chown -R 0:0 /d && chmod 700 /d");
    find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
}

#[test]
fn settings_reach_roots_files_through_the_container() {
    let env = Env::new("remote");
    env.init(&["--no-start"]);
    // As after `zephyr up`: the files are root's, so this user cannot open the dir.
    let files = env.dir.join("files");
    std::fs::create_dir_all(&files).unwrap();
    for name in ["zephyr.toml", "secrets.env", "tui.token", "host.toml"] {
        std::fs::rename(env.daemon().join(name), files.join(name)).unwrap();
    }
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o000)).unwrap();

    env.ok(&["config", "set", "budget.daily_tokens", "5"]);
    let calls = env.calls();
    let get = ["zephyr", "__file", "get", "zephyr.toml"];
    let put = ["zephyr", "__file", "put", "zephyr.toml"];
    let root = ["docker", "exec", "-i", "-u", "0", CONTAINER];
    find(&calls, &[&root[..], &get[..]].concat());
    find(&calls, &[&root[..], &put[..]].concat());
    let text = std::fs::read_to_string(files.join("zephyr.toml")).unwrap();
    assert!(text.contains("daily_tokens = 5"), "{text}");

    // Stopped: a throwaway root container with no capabilities and no network.
    let out = env.zephyr_env(
        &["config", "get", "budget.daily_tokens"],
        &[("SHIM_STATE", "exited")],
    );
    assert_eq!(stdout(&out).trim(), "5");
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "--rm", "-i", "-u", "0"]);
    for pair in [["--cap-drop", "ALL"], ["--network", "none"]] {
        assert!(run.windows(2).any(|w| w == pair), "{pair:?} in {run:?}");
    }
    assert!(run.ends_with(&get.map(String::from)), "{run:?}");
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn up_and_upgrade_add_the_images_new_extensions_through_the_container() {
    let env = Env::new("up-ext");
    env.init(&["--no-start"]);
    // A config written before the MCP adapter shipped and before host.toml recorded what was
    // offered, in root's dir as after the first `up`.
    let adapter = "\"/opt/zephyr/ext/librpi_mcp_adapter.so\"";
    let files = env.dir.join("files");
    std::fs::create_dir_all(&files).unwrap();
    for name in ["zephyr.toml", "secrets.env", "tui.token", "host.toml"] {
        std::fs::rename(env.daemon().join(name), files.join(name)).unwrap();
    }
    let (toml, host) = (files.join("zephyr.toml"), files.join("host.toml"));
    let read = |p: &Path| std::fs::read_to_string(p).unwrap();
    let full = read(&toml);
    let without = full.replace(&format!("{adapter}, "), "");
    assert!(!without.contains(adapter));
    let record = read(&host);
    assert!(record.contains(adapter), "{record}");
    let unrecorded: String = record
        .lines()
        .filter(|l| !l.starts_with("offered_extensions"))
        .map(|l| format!("{l}\n"))
        .collect();
    std::fs::write(&host, &unrecorded).unwrap();
    std::fs::write(&toml, &without).unwrap();
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o000)).unwrap();

    // `up` removes the container first, so the files go through a throwaway root container.
    let up = |cmd: &str, extra: &[(&str, &str)]| {
        let env_vars = [&[("SHIM_STATE", "exited")][..], extra].concat();
        let out = env.zephyr_env(&[cmd], &env_vars);
        assert!(
            out.status.success(),
            "{cmd}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        out
    };
    let put = |name: &str| ["zephyr", "__file", "put", name].map(String::from);
    let throwaway = |calls: &[Vec<String>], name: &str| {
        calls.iter().any(|c| {
            c.starts_with(&["docker", "run", "--rm", "-i", "-u", "0"].map(String::from))
                && c.windows(2).any(|w| w == ["--cap-drop", "ALL"])
                && c.ends_with(&put(name))
        })
    };
    let wrote = |calls: &[Vec<String>]| calls.iter().any(|c| c.ends_with(&put("zephyr.toml")));

    // No record yet: what the config lists counts as offered, so the adapter is new to it.
    up("up", &[]);
    assert_eq!(read(&toml), full);
    assert!(read(&host).contains(adapter));
    let calls = env.calls();
    assert!(throwaway(&calls, "zephyr.toml"), "{calls:#?}");
    assert!(throwaway(&calls, "host.toml"), "{calls:#?}");

    // The owner removes it: it stays removed, and the file is left alone.
    std::fs::write(&toml, &without).unwrap();
    for cmd in ["up", "upgrade"] {
        up(cmd, &[]);
        assert_eq!(read(&toml), without, "{cmd}");
        assert!(!wrote(&env.calls()), "{cmd}");
    }

    // An image that ships it newly (the record lacks it) adds it once, then respects a removal.
    let older = record.replace(&format!("{adapter}, "), "");
    assert_ne!(older, record);
    std::fs::write(&host, &older).unwrap();
    up("upgrade", &[]);
    assert_eq!(read(&toml), full);
    assert!(read(&host).contains(adapter));
    let calls = env.calls();
    assert!(throwaway(&calls, "zephyr.toml") && throwaway(&calls, "host.toml"));
    std::fs::write(&toml, &without).unwrap();
    up("up", &[]);
    assert_eq!(read(&toml), without);

    // A failed write only warns: the container still starts.
    std::fs::write(&host, &older).unwrap();
    env.calls();
    let out = up("up", &[("SHIM_PUT_FAIL", "1")]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("could not add the image's new extensions"),
        "{err}"
    );
    assert_eq!(read(&toml), without);
    find(&env.calls(), &["docker", "run", "-d", "--name", CONTAINER]);
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn doctor_fix_repairs_modes() {
    let env = Env::new("doctor");
    env.init(&["--no-start"]);
    let secrets = env.daemon().join("secrets.env");
    std::fs::set_permissions(&secrets, std::fs::Permissions::from_mode(0o644)).unwrap();
    let out = env.zephyr(&["doctor"]);
    assert!(!out.status.success());
    assert!(stdout(&out).contains(&format!("FAIL {} is private", secrets.display())));
    env.ok(&["doctor", "--fix"]);
    assert_eq!(mode(&secrets), 0o600);
}
