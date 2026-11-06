//! Host commands against a fake `docker` (and `gh`) on PATH: the real Docker is never called.

use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const CONTAINER: &str = "zephyr-lanetest";
/// The agent's PATH, passed to each exec as the agent: the image's own is root's.
const AGENT_PATH: &str =
    "/zephyr/.local/bin:/opt/zephyr/py/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";
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
  inspect) [ -n "$SHIM_NO_CONTAINER" ] && exit 1; echo "${SHIM_STATE:-running}"; exit 0 ;;
  info) echo "${SHIM_RUNTIMES:-io.containerd.runc.v2 runc}"; exit 0 ;;
  image) exit "${SHIM_IMAGE_MISSING:-0}" ;;
  pull) exit "${SHIM_PULL_FAIL:-0}" ;;
  # `docker build -t TAG -`: the Dockerfile on stdin goes to build.log.
  build) [ "${@: -1}" = - ] && cat >> "$SHIM_DIR/build.log"; exit "${SHIM_BUILD_FAIL:-0}" ;;
  run)
    case " $* " in
      *" -d --name "*) exit "${SHIM_RUN_FAIL:-0}" ;;
      *" --entrypoint zephyr "*) echo "zephyr $SHIM_LATEST" ;;
      # The image's CLI: names its image's tag, and logs how it was run.
      *" --entrypoint cat "*) printf '#!/bin/bash\n[ "$1" = --version ] && { echo "zephyr %s"; exit; }\necho "$@" >> "%s/exec.log"\n' "${5##*:}" "$SHIM_DIR" ;;
      *" --entrypoint python3 "*)
        case "${@: -1}" in
          *"os.path.exists(p) or sys.exit(1)"*) [ -n "$SHIM_HAS_BAK" ]; exit ;;
          *".backup("*) exit "${SHIM_PY_FAIL:-0}" ;;
        esac ;;
    esac ;;
  exec)
    case " $* " in *" -i "*) cat >> "$SHIM_DIR/stdin.log" ;; esac
    # The daemon edits zephyr.toml while a sign-in or the model picker waits on the owner.
    case " $* " in *" auth login "*|*" --list-models "*)
      [ -n "$SHIM_EDIT" ] && echo "$SHIM_EDIT" >> "$HOME/.zephyr-daemon/zephyr.toml" ;;
    esac
    case " $* " in
      *" --list-models "*) printf 'provider model context\nanthropic claude-sonnet-5 200K\nopenai-codex gpt-5.5 400K\nopenai-codex gpt-5.4 400K\n' ;;
      *" auth check "*) echo '{"ready":true,"provider":"claude-code","source":"auth.json"}' ;;
      *" zephyr status "*) echo "daemon: ${SHIM_DAEMON:-running}"; [ -n "$SHIM_STATUS" ] && printf '%b\n' "$SHIM_STATUS"; exit "${SHIM_STATUS_EXIT:-0}" ;;
      *" zephyr --version "*) [ -n "$SHIM_VERSION" ] || exit 2; echo "zephyr $SHIM_VERSION" ;;
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

/// `systemctl`, `loginctl` and `curl`: logged like `docker`, never the real ones. `loginctl`
/// answers `Linger=$SHIM_LINGER` (default no); `curl` exits `$SHIM_CURL_FAIL` (default 0).
const SYSTEMCTL: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "systemctl$IFS$*") >> "$SHIM_DIR/calls.log"
"#;

const LOGINCTL: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "loginctl$IFS$*") >> "$SHIM_DIR/calls.log"
echo "Linger=${SHIM_LINGER:-no}"
"#;

const CURL: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "curl$IFS$*") >> "$SHIM_DIR/calls.log"
exit "${SHIM_CURL_FAIL:-0}"
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
        for (name, body) in [
            ("docker", DOCKER),
            ("gh", GH),
            ("systemctl", SYSTEMCTL),
            ("loginctl", LOGINCTL),
            ("curl", CURL),
        ] {
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
        self.run_cli(Path::new(env!("CARGO_BIN_EXE_zephyr")), args, env)
    }

    /// A copy of the CLI, for `zephyr upgrade` to replace instead of target/debug/zephyr, which
    /// every test runs. Copied by cp: a child another test forks mid-copy would inherit this
    /// process's write fd and make exec'ing the copy fail with ETXTBSY.
    fn cli_copy(&self) -> PathBuf {
        let cli = self.dir.join("cli/zephyr");
        std::fs::create_dir_all(cli.parent().unwrap()).unwrap();
        let cp = Command::new("cp")
            .arg(env!("CARGO_BIN_EXE_zephyr"))
            .arg(&cli)
            .status();
        assert!(cp.unwrap().success());
        cli
    }

    fn run_cli(&self, cli: &Path, args: &[&str], env: &[(&str, &str)]) -> Output {
        self.run_input(cli, args, env, None)
    }

    /// `zephyr <args>` with `input` on its stdin (not a terminal).
    fn zephyr_input(&self, args: &[&str], input: &str) -> Output {
        let cli = Path::new(env!("CARGO_BIN_EXE_zephyr"));
        self.run_input(cli, args, &[], Some(input))
    }

    fn run_input(
        &self,
        cli: &Path,
        args: &[&str],
        env: &[(&str, &str)],
        input: Option<&str>,
    ) -> Output {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let mut child = Command::new(cli)
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("SHIM_DIR", &self.dir)
            .env("ZEPHYR_CONTAINER", CONTAINER)
            .env("ZEPHYR_IMAGE", "zephyr-lanetest")
            .envs(env.iter().copied())
            .stdin(match input {
                Some(_) => std::process::Stdio::piped(),
                None => std::process::Stdio::null(),
            })
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(input) = input {
            let mut stdin = child.stdin.take().unwrap();
            std::io::Write::write_all(&mut stdin, input.as_bytes()).unwrap();
        }
        let out = child.wait_with_output().unwrap();
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

    /// The layout after the first `up`: the daemon's files root's, reached through `__file` in the
    /// container (the shim serves them from `files/`). `unsplit` gives the dir back.
    fn split(&self) {
        let files = self.dir.join("files");
        std::fs::create_dir_all(&files).unwrap();
        for name in ["zephyr.toml", "secrets.env", "tui.token", "host.toml"] {
            std::fs::rename(self.daemon().join(name), files.join(name)).unwrap();
        }
        std::fs::set_permissions(self.daemon(), std::fs::Permissions::from_mode(0o000)).unwrap();
    }

    fn unsplit(&self) {
        std::fs::set_permissions(self.daemon(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }

    fn init(&self, extra: &[&str]) {
        let projects = self.projects.display().to_string();
        let mut args = vec!["init", "--non-interactive", "--projects", &projects];
        args.extend(extra);
        self.ok(&args);
    }
}

impl Drop for Env {
    /// Unlocks what a test locked, even when it panicked, then removes the dir (kept on failure).
    fn drop(&mut self) {
        for (d, m) in [
            (self.daemon(), 0o700),
            (self.dir.join("cli"), 0o755),
            (self.projects.join("pgdata"), 0o700),
        ] {
            let _ = std::fs::set_permissions(d, std::fs::Permissions::from_mode(m));
        }
        if !std::thread::panicking() {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
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
    // SAFETY: getuid and getgid have no arguments and cannot fail.
    unsafe { format!("{}:{}", libc::getuid(), libc::getgid()) }
}

/// A `docker exec` as the agent (this user, in its home) whose argv starts with `rest`.
fn find_agent<'a>(calls: &'a [Vec<String>], rest: &[&str]) -> &'a Vec<String> {
    let ids = ids();
    let path = format!("PATH={AGENT_PATH}");
    let prefix: Vec<&str> = ["docker", "exec", "-u", &ids, "-w", "/zephyr", "-e", &path]
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
    for pair in [
        ["--log-opt", "max-size=20m"],
        ["--log-opt", "max-file=5"],
        ["--ulimit", "nofile=65536:65536"],
    ] {
        assert!(run.windows(2).any(|w| w == pair), "{pair:?} in {run:?}");
    }
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
fn up_caps_memory_at_host_toml_or_three_quarters_of_the_host() {
    let env = Env::new("memory");
    env.init(&["--no-start"]);
    // Unset: 75% of this host's MemTotal.
    env.ok(&["up"]);
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    let info = std::fs::read_to_string("/proc/meminfo").unwrap();
    let kb: u64 = info
        .lines()
        .find_map(|l| l.strip_prefix("MemTotal:"))
        .unwrap()
        .trim()
        .trim_end_matches("kB")
        .trim()
        .parse()
        .unwrap();
    let want = format!("{}k", kb / 4 * 3);
    assert!(run.windows(2).any(|w| w == ["--memory", &want]), "{run:?}");
    // Set: the owner's value.
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
fn alert_installs_a_user_timer_and_posts_only_when_health_changes() {
    let env = Env::new("alert");
    env.init(&["--no-start"]);
    let out = env.ok(&["alert", "setup", "zephyr-lane_1"]);
    assert!(out.contains("loginctl enable-linger $USER"), "{out}");
    let units = env.home.join(".config/systemd/user");
    let service = std::fs::read_to_string(units.join("zephyr-alert.service")).unwrap();
    let exe = std::fs::canonicalize(env!("CARGO_BIN_EXE_zephyr")).unwrap();
    assert!(
        service.contains(&format!("\nExecStart={} alert check\n", exe.display())),
        "{service}"
    );
    assert!(service.contains("Type=oneshot"), "{service}");
    assert!(service.contains(&format!("Environment=ZEPHYR_CONTAINER={CONTAINER}")));
    let timer = std::fs::read_to_string(units.join("zephyr-alert.timer")).unwrap();
    assert!(timer.contains("OnCalendar=*:0/5"), "{timer}");
    assert!(timer.contains("WantedBy=timers.target"), "{timer}");
    let host: toml::Value =
        toml::from_str(&std::fs::read_to_string(env.daemon().join("host.toml")).unwrap()).unwrap();
    assert_eq!(host["alert"]["topic"].as_str(), Some("zephyr-lane_1"));
    assert_eq!(host["alert"]["server"].as_str(), Some("https://ntfy.sh"));
    let calls = env.calls();
    find(&calls, &["systemctl", "--user", "daemon-reload"]);
    find(
        &calls,
        &[
            "systemctl",
            "--user",
            "enable",
            "--now",
            "zephyr-alert.timer",
        ],
    );
    let uid = ids().split(':').next().unwrap().to_string();
    find(&calls, &["loginctl", "show-user", &uid, "-p", "Linger"]);
    // Lingering on: no hint.
    let out = env.zephyr_env(
        &["alert", "setup", "zephyr-lane_1"],
        &[("SHIM_LINGER", "yes")],
    );
    assert!(!stdout(&out).contains("enable-linger"));
    assert!(
        !env.zephyr(&["alert", "setup", "a/b"]).status.success(),
        "a topic is checked"
    );
    env.calls();

    let url = "https://ntfy.sh/zephyr-lane_1";
    let posts = |env: &Env| -> Vec<Vec<String>> {
        env.calls().into_iter().filter(|c| c[0] == "curl").collect()
    };
    let check = |vars: &[(&str, &str)]| {
        let out = env.zephyr_env(&["alert", "check"], vars);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    check(&[]);
    assert!(
        posts(&env).is_empty(),
        "healthy from the start: nothing to say"
    );
    // Stopped: one post, however many checks find it so.
    check(&[("SHIM_STATE", "exited")]);
    check(&[("SHIM_STATE", "exited")]);
    let p = posts(&env);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(
        p[0].windows(2)
            .any(|w| w == ["-H", "Title: zephyr is down"]),
        "{p:?}"
    );
    let body = &p[0][p[0].iter().position(|a| a == "-d").unwrap() + 1];
    assert_eq!(
        body,
        &format!("zephyr is unhealthy: container {CONTAINER} is exited")
    );
    assert_eq!(p[0].last().unwrap(), url);
    assert!(p[0].contains(&"-fsS".to_string()));
    // Still down, for another reason: nothing new.
    check(&[("SHIM_STATUS_EXIT", "1")]);
    assert!(posts(&env).is_empty());
    check(&[]);
    let p = posts(&env);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(p[0].contains(&"zephyr is back".to_string()), "{p:?}");
    // `zephyr status` failing, then replies failing, each after a recovery.
    check(&[("SHIM_STATUS_EXIT", "1")]);
    let p = posts(&env);
    assert!(
        p[0].iter()
            .any(|a| a.starts_with("zephyr is unhealthy: zephyr status failed")),
        "{p:?}"
    );
    check(&[]);
    posts(&env);
    check(&[(
        "SHIM_STATUS",
        "outbox  sent        4\noutbox  failed      2",
    )]);
    let p = posts(&env);
    assert!(
        p[0].contains(&"zephyr is unhealthy: 2 replies failed to send".to_string()),
        "{p:?}"
    );
    check(&[]);
    posts(&env);
    // A post that fails is made again at the next check.
    let out = env.zephyr_env(
        &["alert", "check"],
        &[("SHIM_STATE", "exited"), ("SHIM_CURL_FAIL", "22")],
    );
    assert!(!out.status.success());
    check(&[("SHIM_STATE", "exited")]);
    assert_eq!(posts(&env).len(), 2);

    env.ok(&["alert", "test"]);
    let p = posts(&env);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].last().unwrap(), url);

    env.ok(&["alert", "off"]);
    let calls = env.calls();
    find(
        &calls,
        &[
            "systemctl",
            "--user",
            "disable",
            "--now",
            "zephyr-alert.timer",
        ],
    );
    find(&calls, &["systemctl", "--user", "daemon-reload"]);
    assert!(!units.join("zephyr-alert.service").exists());
    assert!(!units.join("zephyr-alert.timer").exists());
}

#[test]
fn packages_build_a_local_image_that_up_and_upgrade_run() {
    let env = Env::new("packages");
    env.init(&["--no-start"]);
    let host = env.daemon().join("host.toml");
    assert_eq!(
        env.ok(&["packages", "list"]),
        "no packages (zephyr packages add NAME)\n"
    );

    // Names are checked before anything runs.
    for bad in ["Foo", "x;id", "git-"] {
        assert!(
            !env.zephyr(&["packages", "add", bad]).status.success(),
            "{bad}"
        );
    }
    // A failed build saves nothing.
    let out = env.zephyr_env(
        &["packages", "add", "nosuchpkg"],
        &[("SHIM_IMAGE_MISSING", "1"), ("SHIM_BUILD_FAIL", "1")],
    );
    assert!(!out.status.success());
    assert!(!read(&host).contains("packages ="));
    let _ = std::fs::remove_file(env.dir.join("build.log"));
    env.calls();

    // add: built from stdin on the image, saved sorted, and the running container recreated.
    let out = env.zephyr_env(
        &["packages", "add", "jq", "ffmpeg"],
        &[("SHIM_IMAGE_MISSING", "1")],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        read(&host).contains(r#"packages = ["ffmpeg", "jq"]"#),
        "{}",
        read(&host)
    );
    assert_eq!(
        read(&env.dir.join("build.log")),
        "FROM zephyr-lanetest\nRUN apt-get update && apt-get install -y --no-install-recommends \
         ffmpeg jq && rm -rf /var/lib/apt/lists/*\n"
    );
    let calls = env.calls();
    let tag = find(&calls, &["docker", "build", "-t"])[3].clone();
    assert!(tag.starts_with("zephyr-local:custom-"), "{tag}");
    let build = at(&calls, &["docker", "build"], "").unwrap();
    let run = at(&calls, RUN, "").unwrap();
    assert!(build < run, "{calls:#?}");
    assert_eq!(calls[run].last().unwrap(), &tag);
    assert_eq!(env.ok(&["packages", "list"]), "ffmpeg\njq\n");
    // Nothing new: no build, no restart.
    env.ok(&["packages", "add", "jq"]);
    assert_eq!(at(&env.calls(), RM, ""), None);

    // up runs the local image, built only when missing.
    env.ok(&["up"]);
    let calls = env.calls();
    assert_eq!(find(&calls, RUN).last().unwrap(), &tag);
    assert_eq!(at(&calls, &["docker", "build"], ""), None);

    // upgrade builds it on the new image before the container goes.
    let me = env!("CARGO_PKG_VERSION");
    let out = env.run_cli(
        &env.cli_copy(),
        &["upgrade", me],
        &[
            ("ZEPHYR_IMAGE", ""),
            ("SHIM_VERSION", "0.0.1"),
            ("SHIM_IMAGE_MISSING", "1"),
        ],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = env.calls();
    let release = format!("ghcr.io/yansigit/zephyr:{me}");
    let build = at(&calls, &["docker", "build", "-t"], "zephyr-local:").expect("a build");
    assert!(calls[build][3].starts_with(&format!("zephyr-local:{me}-")));
    assert!(build < at(&calls, RM, "").unwrap(), "{calls:#?}");
    assert_eq!(find(&calls, RUN).last().unwrap(), &calls[build][3]);
    assert!(read(&env.dir.join("build.log")).contains(&format!("FROM {release}\n")));
    // To a newer release too, before its CLI takes over.
    let vars = [
        ("ZEPHYR_IMAGE", ""),
        ("SHIM_LATEST", "99.0.0"),
        ("SHIM_VERSION", me),
        ("SHIM_IMAGE_MISSING", "1"),
    ];
    assert!(env
        .run_cli(&env.cli_copy(), &["upgrade"], &vars)
        .status
        .success());
    let calls = env.calls();
    let build = at(&calls, &["docker", "build", "-t"], "zephyr-local:99.0.0-").expect("a build");
    assert!(
        build
            < at(
                &calls,
                &["docker", "run", "--rm", "--entrypoint", "cat"],
                ""
            )
            .unwrap()
    );

    // rm of the last ones goes back to the plain image, with no build.
    env.ok(&["packages", "rm", "ffmpeg", "jq"]);
    assert!(!read(&host).contains("packages ="));
    let calls = env.calls();
    assert_eq!(at(&calls, &["docker", "build"], ""), None);
    assert_eq!(find(&calls, RUN).last().unwrap(), "zephyr-lanetest");
    assert!(!env.zephyr(&["packages", "rm", "jq"]).status.success());

    // Stopped: saved, applied by the next `zephyr up`.
    let out = env.zephyr_env(&["packages", "add", "jq"], &[("SHIM_STATE", "exited")]);
    assert!(
        stdout(&out).contains("applies when zephyr starts"),
        "{}",
        stdout(&out)
    );
    assert_eq!(at(&env.calls(), RUN, ""), None);
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
        login[14].starts_with("rpi auth login --provider claude-code --extension /opt/zephyr/ext/"),
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
fn subagents_and_task_settings_edit_the_toml() {
    let env = Env::new("subagent");
    env.init(&["--no-start"]);
    env.ok(&["config", "set", "tasks.parallel", "16"]);
    env.ok(&[
        "config",
        "set",
        "tasks.model",
        "claude-code/claude-haiku-5-5",
    ]);
    assert!(!env
        .zephyr(&["config", "set", "tasks.model", "haiku"])
        .status
        .success());
    let add = [
        "subagent",
        "add",
        "explorer",
        "--model",
        "claude-code/claude-haiku-5-5",
        "--prompt",
        "You explore and report; you never edit files.",
    ];
    env.ok(&add);
    // Without --prompt, the prompt comes from stdin.
    let out = env.zephyr_input(
        &["subagent", "add", "implementer"],
        "You implement.\nRun the tests.\n",
    );
    assert!(out.status.success(), "{out:?}");
    assert_eq!(
        env.ok(&["subagent", "list"]),
        "explorer (claude-code/claude-haiku-5-5): You explore and report; you never edit files.\n\
         implementer (default): You implement.\n\
         tasks: 16 at once, on claude-code/claude-haiku-5-5\n"
    );
    find(
        &env.calls(),
        &["docker", "exec", CONTAINER, "zephyr", "reload"],
    );
    env.ok(&[
        "subagent",
        "edit",
        "implementer",
        "--model",
        "opencode-go/deepseek-4.1-flash",
    ]);
    env.ok(&["subagent", "edit", "explorer", "--model", "default"]);
    let before = env.config();
    for bad in [
        &["subagent", "add", "explorer", "--prompt", "again"][..],
        &["subagent", "add", "Bad", "--prompt", "p"],
        &["subagent", "add", "x"],
        &["subagent", "add", "x", "--model", "haiku", "--prompt", "p"],
        &["subagent", "edit", "nope", "--prompt", "p"],
        &["subagent", "edit", "explorer"],
        &["subagent", "rm", "nope"],
    ] {
        assert!(!env.zephyr(bad).status.success(), "{bad:?}");
    }
    assert_eq!(env.config(), before, "a refused edit writes nothing");
    env.ok(&["subagent", "rm", "explorer"]);
    let config = env.config();
    assert!(config.contains("[tasks]\nparallel = 16"), "{config}");
    assert!(!config.contains("explorer"), "{config}");
    assert!(
        config.contains("name = \"implementer\"\nprompt = \"\"\"\nYou implement.\nRun the tests.\"\"\"\nmodel = \"opencode-go/deepseek-4.1-flash\""),
        "{config}"
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
    assert!(
        tui.contains(&"HOME=/var/lib/zephyr/tui".to_string()),
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
            "HOME=/var/lib/zephyr/tui",
            "-e",
            "RPI_CODING_AGENT_DIR=/var/lib/zephyr/tui/agent",
            CONTAINER,
            "rpi",
            "--connect",
            "127.0.0.1:7790",
        ],
    );
    // In the daemon's dir, which the agent cannot enter, not in /tmp, where it could make it first.
    find(
        &calls,
        &[
            "docker",
            "exec",
            CONTAINER,
            "mkdir",
            "-p",
            "-m",
            "0700",
            "/var/lib/zephyr/tui",
        ],
    );
    assert!(!format!("{calls:?}").contains("/tmp"), "{calls:#?}");
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

/// `zephyr shell` is a login shell as the agent, in its home, on the desktop; `zsh` picks zsh.
#[test]
fn shell_opens_a_login_shell_as_the_agent() {
    let env = Env::new("shell");
    env.init(&["--no-start"]);
    env.calls();
    let out = env.zephyr(&["shell"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("needs a terminal"));
    if Command::new("script").arg("--version").output().is_err() {
        return eprintln!("skipped: no script(1) to give zephyr a terminal");
    }
    for (args, sh) in [("shell", "bash"), ("shell zsh", "zsh")] {
        let out = env.on_a_tty(args, &[]);
        assert!(out.status.success(), "{}", stdout(&out));
        let calls = env.calls();
        let call = find_agent(&calls, &["-it", "-e", "TERM"]);
        let tail = [
            "-e",
            "DISPLAY=:99",
            "-e",
            "XAUTHORITY=/zephyr/.zephyr-xauthority",
            CONTAINER,
            sh,
            "-l",
        ];
        assert!(call.ends_with(&tail.map(String::from)), "{call:?}");
    }
    assert!(!env.zephyr(&["shell", "fish"]).status.success());
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

#[test]
fn version_names_the_cli_and_the_running_container() {
    let env = Env::new("version");
    let me = env!("CARGO_PKG_VERSION");
    let out = env.zephyr_env(&["--version"], &[("SHIM_VERSION", me)]);
    assert!(out.status.success());
    assert_eq!(
        stdout(&out),
        format!("zephyr {me}\ncontainer {CONTAINER}: {me}\n")
    );
    let older = stdout(&env.zephyr_env(&["-V"], &[("SHIM_VERSION", "0.0.1")]));
    assert!(
        older.ends_with(
            ": 0.0.1 (not this CLI's; `zephyr upgrade` brings both to the newest release)\n"
        ),
        "{older}"
    );
    // An image without --version is named by its image instead.
    let old = stdout(&env.zephyr(&["--version"]));
    assert_eq!(old.lines().count(), 2, "{old}");
    let stopped = env.zephyr_env(&["--version"], &[("SHIM_STATE", "exited")]);
    assert_eq!(stdout(&stopped), format!("zephyr {me}\n"));
}

#[test]
fn channel_list_and_remove_edit_the_toml_and_secrets() {
    let env = Env::new("channel");
    env.init(&["--no-start"]);
    assert_eq!(env.ok(&["channel", "list"]), "chat     owners: 0\n");

    // Both channels, as `channel add` leaves them; the owners go in through `config set`.
    env.ok(&["config", "set", "owners.discord", "[\"1234\"]"]);
    env.ok(&["config", "set", "discord.token_env", "ZEPHYR_DISCORD_TOKEN"]);
    env.ok(&["config", "set", "owners.imessage", "[\"+15550100\"]"]);
    let mut cfg = env.config();
    cfg.push_str(
        "\n[imessage]\nlisten = \"100.64.0.1:8080\"\nserver_url = \"http://100.64.0.2:1234\"\n\
         password_env = \"ZEPHYR_BB_PASSWORD\"\nsecret_env = \"ZEPHYR_BB_SECRET\"\n",
    );
    std::fs::write(env.daemon().join("zephyr.toml"), cfg).unwrap();
    let secrets = env.daemon().join("secrets.env");
    std::fs::write(
        &secrets,
        "ZEPHYR_DISCORD_TOKEN=d\nZEPHYR_BB_PASSWORD=pw\nZEPHYR_BB_SECRET=s\nOTHER=keep\n",
    )
    .unwrap();
    assert_eq!(
        env.ok(&["channel", "list"]),
        "chat     owners: 0\ndiscord  owners: 1234\nimessage owners: +15550100\n"
    );

    env.calls();
    env.ok(&["channel", "remove", "discord"]);
    find(
        &env.calls(),
        &["docker", "exec", CONTAINER, "zephyr", "reload"],
    );
    let text = env.config();
    let cfg: toml::Value = toml::from_str(&text).unwrap();
    assert!(cfg.get("discord").is_none(), "{text}");
    assert!(cfg["owners"].get("discord").is_none(), "{text}");
    assert_eq!(cfg["owners"]["imessage"][0].as_str(), Some("+15550100"));
    assert!(text.contains("# Zephyr's config"), "{text}");
    assert_eq!(cfg["attach"]["port"].as_integer(), Some(7790));
    let left = std::fs::read_to_string(&secrets).unwrap();
    assert_eq!(
        left,
        "ZEPHYR_BB_PASSWORD=pw\nZEPHYR_BB_SECRET=s\nOTHER=keep\n"
    );

    let again = env.zephyr(&["channel", "remove", "discord"]);
    assert!(!again.status.success());
    assert!(String::from_utf8_lossy(&again.stderr).contains("discord is not configured"));

    env.ok(&["channel", "remove", "imessage"]);
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert!(cfg.get("imessage").is_none() && cfg["owners"].get("imessage").is_none());
    assert_eq!(std::fs::read_to_string(&secrets).unwrap(), "OTHER=keep\n");
    assert_eq!(env.ok(&["channel", "list"]), "chat     owners: 0\n");
}

#[test]
fn help_prints_usage_on_stdout_and_exits_0() {
    let env = Env::new("help");
    for arg in ["--help", "-h", "help"] {
        let out = env.zephyr(&[arg]);
        assert!(out.status.success(), "{arg}");
        assert_eq!(stdout(&out), format!("{}\n", zephyr::host::USAGE), "{arg}");
    }
    let cases: [&[&str]; 2] = [&["bogus"], &[]];
    for args in cases {
        let out = env.zephyr(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
        assert!(out.stdout.is_empty(), "{args:?}");
        assert!(String::from_utf8_lossy(&out.stderr).starts_with("usage: zephyr <command>"));
    }
}

/// Where the first call starting with `prefix` and containing `has` is, if any.
fn at(calls: &[Vec<String>], prefix: &[&str], has: &str) -> Option<usize> {
    calls.iter().position(|c| {
        c.len() >= prefix.len() && c[..prefix.len()] == *prefix && c.iter().any(|a| a.contains(has))
    })
}

const PY: &[&str] = &["docker", "run", "--rm", "--user", "0"];
const RM: &[&str] = &["docker", "rm", "-f"];
const RUN: &[&str] = &["docker", "run", "-d"];

#[test]
fn upgrade_replaces_the_cli_from_the_newer_image_first() {
    let env = Env::new("upgrade-cli");
    let me = env!("CARGO_PKG_VERSION");
    let cli = env.cli_copy();
    let vars = [
        ("ZEPHYR_IMAGE", ""),
        ("SHIM_LATEST", "99.0.0"),
        ("SHIM_VERSION", me),
    ];
    let out = env.run_cli(&cli, &["upgrade"], &vars);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains(&format!("zephyr CLI: {me} -> 99.0.0")));
    // The image's CLI took this one's place and finishes the upgrade.
    assert_eq!(read(&env.dir.join("exec.log")), "upgrade 99.0.0\n");
    assert!(read(&cli).contains("exec.log"));
    let calls = env.calls();
    find(
        &calls,
        &["docker", "pull", "ghcr.io/yansigit/zephyr:latest"],
    );
    find(
        &calls,
        &[
            "docker",
            "run",
            "--rm",
            "--entrypoint",
            "cat",
            "ghcr.io/yansigit/zephyr:99.0.0",
        ],
    );
    assert_eq!(at(&calls, RM, ""), None, "{calls:#?}");

    // A CLI it cannot replace names the command that does it by hand; nothing else changes.
    let cli = env.cli_copy();
    let dir = cli.parent().unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o555)).unwrap();
    let out = env.run_cli(&cli, &["upgrade"], &vars);
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!(
            "docker run --rm --entrypoint cat ghcr.io/yansigit/zephyr:99.0.0 /usr/local/bin/zephyr > {}",
            cli.display()
        )),
        "{err}"
    );
    assert_eq!(at(&env.calls(), RM, ""), None);

    // No `latest` to read: an error, never a local build.
    let out = env.run_cli(&cli, &["upgrade"], &[vars[0], ("SHIM_PULL_FAIL", "1")]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr)
        .contains("could not pull ghcr.io/yansigit/zephyr:latest"));
    assert_eq!(at(&env.calls(), &["docker", "build"], ""), None);
    assert!(!env.zephyr(&["upgrade", "v1.2"]).status.success());
}

#[test]
fn upgrade_waits_backs_up_and_goes_back_only_with_a_backup() {
    let env = Env::new("upgrade-db");
    env.init(&["--no-start"]);
    let me = env!("CARGO_PKG_VERSION");
    let cli = env.cli_copy();
    let upgrade = |vars: &[(&str, &str)]| {
        let vars = [&[("ZEPHYR_IMAGE", "")][..], vars].concat();
        let out = env.run_cli(&cli, &["upgrade", me], &vars);
        (out, env.calls())
    };

    // Already there: nothing restarts.
    let (out, calls) = upgrade(&[("SHIM_VERSION", me)]);
    assert!(out.status.success());
    assert_eq!(stdout(&out), format!("already on {me}\n"));
    assert_eq!(at(&calls, RM, ""), None);

    // A release never comes from a local build: missing and not pullable, it stops.
    let (out, calls) = upgrade(&[
        ("SHIM_VERSION", "0.0.1"),
        ("SHIM_IMAGE_MISSING", "1"),
        ("SHIM_PULL_FAIL", "1"),
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("could not pull"));
    assert_eq!(at(&calls, &["docker", "build"], ""), None);
    assert_eq!(at(&calls, RM, ""), None);

    // Forward: state.db is backed up under the running version before the container goes.
    let (out, calls) = upgrade(&[("SHIM_VERSION", "0.0.1"), ("SHIM_IMAGE_MISSING", "1")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let backup = at(&calls, PY, "connect('/var/lib/zephyr/state.db.0.0.1.bak')").expect("backup");
    assert!(backup < at(&calls, RM, "").unwrap() && backup < at(&calls, RUN, "").unwrap());
    assert!(stdout(&out).contains(&format!(
        "state.db backed up to {}",
        env.daemon().join("state.db.0.0.1.bak").display()
    )));
    find(
        &calls,
        &["docker", "pull", &format!("ghcr.io/yansigit/zephyr:{me}")],
    );

    // A failed backup stops it.
    let (out, calls) = upgrade(&[("SHIM_VERSION", "0.0.1"), ("SHIM_PY_FAIL", "1")]);
    assert!(!out.status.success());
    assert_eq!(at(&calls, RM, ""), None);

    // Going back needs that version's backup.
    let (out, calls) = upgrade(&[("SHIM_VERSION", "99.0.0")]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!("no backup from {me} (state.db.{me}.bak)")),
        "{err}"
    );
    assert_eq!(at(&calls, RM, ""), None);
    let (out, calls) = upgrade(&[("SHIM_VERSION", "99.0.0"), ("SHIM_HAS_BAK", "1")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let backup = at(&calls, PY, "connect('/var/lib/zephyr/state.db.99.0.0.bak')").expect("backup");
    let stop = at(&calls, &["docker", "stop"], "").unwrap();
    let rm = at(&calls, RM, "").unwrap();
    let restore = at(&calls, PY, &format!("shutil.copy(d + 'state.db.{me}.bak'")).expect("restore");
    let run = at(&calls, RUN, "").unwrap();
    assert!(
        backup < stop && stop < rm && rm < restore && restore < run,
        "{calls:#?}"
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("changes since then are lost"));
    // The image is here already: going back pulls nothing.
    assert_eq!(at(&calls, &["docker", "pull"], ""), None, "{calls:#?}");

    // The scripts themselves, on a copy of the daemon's dir: the config goes with state.db, and
    // only the newest 5 versions' backups stay.
    let script = |at: usize| calls[at].last().unwrap().clone();
    let (backup, restore) = (script(backup), script(restore));
    let dir = env.dir.join("daemon-copy");
    std::fs::create_dir_all(&dir).unwrap();
    let py = |script: &str| {
        let script = script.replace("/var/lib/zephyr/", &format!("{}/", dir.display()));
        let out = Command::new("python3")
            .args(["-c", &script])
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    py("import sqlite3; sqlite3.connect('/var/lib/zephyr/state.db').execute('create table t (x)')");
    std::fs::write(dir.join("zephyr.toml"), "newer = 1\n").unwrap();
    std::fs::write(dir.join("host.toml"), "projects = '/p'\n").unwrap();
    let day = std::time::Duration::from_secs(86_400);
    for n in 1..=5 {
        for f in ["state.db", "zephyr.toml"] {
            let file = std::fs::File::create(dir.join(format!("{f}.0.0.{n}.bak"))).unwrap();
            let age = std::time::SystemTime::now() - day * (10 - n);
            file.set_modified(age).unwrap();
        }
    }
    py(&backup);
    let names = |prefix: &str| {
        let mut v: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .filter(|n| n.starts_with(prefix) && n.ends_with(".bak"))
            .collect();
        v.sort();
        v
    };
    assert_eq!(
        names("state.db."),
        ["0.0.2", "0.0.3", "0.0.4", "0.0.5", "99.0.0"].map(|v| format!("state.db.{v}.bak"))
    );
    assert_eq!(
        names("zephyr.toml."),
        ["0.0.2", "0.0.3", "0.0.4", "0.0.5", "99.0.0"].map(|v| format!("zephyr.toml.{v}.bak"))
    );
    assert_eq!(read(&dir.join("zephyr.toml.99.0.0.bak")), "newer = 1\n");
    assert!(dir.join("host.toml.99.0.0.bak").exists());
    assert!(!dir.join("vault.toml.99.0.0.bak").exists());
    std::fs::copy(
        dir.join("state.db.99.0.0.bak"),
        dir.join(format!("state.db.{me}.bak")),
    )
    .unwrap();
    std::fs::write(dir.join(format!("zephyr.toml.{me}.bak")), "older = 1\n").unwrap();
    py(&restore);
    assert_eq!(read(&dir.join("zephyr.toml")), "older = 1\n");
    assert_eq!(
        read(&dir.join("host.toml")),
        "projects = '/p'\n",
        "no backup: left alone"
    );

    // A named image keeps its own: no CLI from it, but the backup still comes first.
    let out = env.zephyr(&["upgrade"]);
    assert!(out.status.success());
    let calls = env.calls();
    assert_eq!(
        at(
            &calls,
            &["docker", "run", "--rm", "--entrypoint", "cat"],
            ""
        ),
        None
    );
    assert!(at(&calls, PY, ".backup").unwrap() < at(&calls, RUN, "").unwrap());
}

fn read(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap()
}

#[test]
fn vault_add_list_and_rm_reach_roots_file_and_never_put_a_value_in_argv() {
    const PASSWORD: &str = "hunter2-host-s3cret";
    const TOKEN: &str = "ghp_host_token_0123";
    let env = Env::new("vault");
    env.init(&["--no-start"]);
    // As after `zephyr up`: the daemon's files are root's, reached through `zephyr __file`.
    let files = env.dir.join("files");
    std::fs::create_dir_all(&files).unwrap();
    for name in ["zephyr.toml", "secrets.env", "tui.token", "host.toml"] {
        std::fs::rename(env.daemon().join(name), files.join(name)).unwrap();
    }
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o000)).unwrap();

    let add = [
        "vault",
        "add",
        "login",
        "github",
        "--site",
        "https://github.com",
        "--user",
        "aiden",
        "--totp",
    ];
    let out = env.zephyr_input(&add, &format!("{PASSWORD}\nJBSWY3DPEHPK3PXP\n"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out = env.zephyr_input(
        &["vault", "add", "token", "GH_TOKEN"],
        &format!("{TOKEN}\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let list = env.ok(&["vault", "list"]);
    assert!(
        list.contains("login github: aiden on https://github.com, with a one-time code"),
        "{list}"
    );
    assert!(
        list.contains("token GH_TOKEN: not allowed for any program yet"),
        "{list}"
    );

    let calls = env.calls();
    let root = ["docker", "exec", "-i", "-u", "0", CONTAINER];
    for op in ["get", "put"] {
        find(
            &calls,
            &[&root[..], &["zephyr", "__file", op, "vault.toml"]].concat(),
        );
    }
    let saved = std::fs::read_to_string(files.join("vault.toml")).unwrap();
    assert!(saved.contains(PASSWORD) && saved.contains(TOKEN));
    let output = std::fs::read_to_string(env.dir.join("output.log")).unwrap();
    for value in [PASSWORD, TOKEN, "JBSWY3DPEHPK3PXP"] {
        assert!(!format!("{calls:?}").contains(value), "{value} in argv");
        assert!(!output.contains(value), "{value} printed");
    }

    // A value never goes in an argument, and a bad one names only the problem.
    assert!(!env
        .zephyr(&["vault", "add", "token", "GH_TOKEN", TOKEN])
        .status
        .success());
    let out = env.zephyr_input(&["vault", "add", "token", "lower"], "v\n");
    assert!(!out.status.success());

    env.ok(&["vault", "rm", "github"]);
    let saved = std::fs::read_to_string(files.join("vault.toml")).unwrap();
    assert!(
        !saved.contains("github") && saved.contains("GH_TOKEN"),
        "{saved}"
    );
    assert!(!env.zephyr(&["vault", "rm", "github"]).status.success());
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o700)).unwrap();
}

#[test]
fn upgrade_waits_only_for_work_in_flight() {
    let env = Env::new("upgrade-busy");
    env.init(&["--no-start"]);
    // A notice for a TUI nobody attaches, messages while paused: these never clear by themselves.
    let out = env.zephyr_env(
        &["upgrade"],
        &[(
            "SHIM_STATUS",
            "outbox  pending     1\\ninbox   new         2",
        )],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(!stdout(&out).contains("the agent is busy"));
    find(&env.calls(), RUN);
}

#[test]
fn up_keeps_the_old_container_until_the_new_one_runs() {
    let env = Env::new("up-order");
    env.init(&["--no-start"]);
    // An unreadable dir under projects (a database's data dir) does not stop it; a comma, which
    // --mount cannot carry, is skipped with a warning.
    let locked = env.projects.join("pgdata");
    std::fs::create_dir_all(env.projects.join("a,b")).unwrap();
    std::fs::write(env.projects.join("a,b/.env"), "S=1").unwrap();
    std::fs::create_dir_all(&locked).unwrap();
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let out = env.zephyr(&["up"]);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(String::from_utf8_lossy(&out.stderr).contains("a,b/.env stays visible"));
    let calls = env.calls();
    let run = find(&calls, RUN);
    assert!(!run.iter().any(|a| a.contains("a,b")), "{run:?}");
    assert!(run.iter().any(|a| a.contains("app/.env")), "{run:?}");
    // Stopped before it is removed, so its daemon drains.
    let stop = at(&calls, &["docker", "stop", CONTAINER], "").unwrap();
    assert!(stop < at(&calls, RM, "").unwrap(), "{calls:#?}");

    // Once split, the old container only steps aside, and comes back when the new one fails.
    env.split();
    let old = format!("{CONTAINER}-old");
    let out = env.zephyr_env(&["up"], &[("SHIM_RUN_FAIL", "1")]);
    assert!(!out.status.success());
    let calls = env.calls();
    let run = at(&calls, RUN, "").unwrap();
    assert!(
        !calls[..run]
            .iter()
            .any(|c| c[..] == ["docker", "rm", "-f", CONTAINER]),
        "{calls:#?}"
    );
    let stop = at(&calls, &["docker", "stop", CONTAINER], "").unwrap();
    let aside = at(&calls, &["docker", "rename", CONTAINER, &old], "").unwrap();
    let back = at(&calls, &["docker", "rename", &old, CONTAINER], "").unwrap();
    let start = at(&calls, &["docker", "start", CONTAINER], "").unwrap();
    assert!(
        stop < aside && aside < run && run < back && back < start,
        "{calls:#?}"
    );
    // It runs: the old one goes only now.
    env.ok(&["up"]);
    let calls = env.calls();
    let run = at(&calls, RUN, "").unwrap();
    let removed = calls
        .iter()
        .rposition(|c| c[..] == ["docker", "rm", "-f", &old])
        .unwrap();
    assert!(run < removed, "{calls:#?}");
    env.unsplit();
}

#[test]
fn up_and_going_back_without_a_container_keep_the_backups() {
    let env = Env::new("up-backup");
    env.init(&["--no-start"]);
    // Another version runs: `up` backs it up before replacing it, as an upgrade does.
    env.ok(&["up"]);
    assert_eq!(at(&env.calls(), PY, ".backup("), None);
    let out = env.zephyr_env(&["up"], &[("SHIM_VERSION", "0.0.1")]);
    assert!(out.status.success());
    let calls = env.calls();
    let backup = at(&calls, PY, "connect('/var/lib/zephyr/state.db.0.0.1.bak')").expect("backup");
    assert!(backup < at(&calls, RUN, "").unwrap(), "{calls:#?}");

    // After `zephyr down` no container names the version, so this CLI's stands in: going back
    // still needs the older version's backup.
    let out = env.run_cli(
        &env.cli_copy(),
        &["upgrade", "0.0.1"],
        &[("ZEPHYR_IMAGE", ""), ("SHIM_NO_CONTAINER", "1")],
    );
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("no backup from 0.0.1"), "{err}");
    assert_eq!(at(&env.calls(), RUN, ""), None);
}

#[test]
fn host_edits_keep_what_the_daemon_wrote_meanwhile() {
    let env = Env::new("reread");
    env.init(&["--no-start"]);
    let edit = "# the daemon was here";
    let out = env.zephyr_env(
        &["login", "anthropic", "--no-verify"],
        &[("ANTHROPIC_API_KEY", "sk-fake"), ("SHIM_EDIT", edit)],
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(env.config().contains(edit), "{}", env.config());
    assert!(env.config().contains("anthropic-1"), "{}", env.config());
    let edit = "# and again";
    let out = env.zephyr_env(&["model"], &[("SHIM_EDIT", edit)]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(env.config().contains(edit), "{}", env.config());
}
