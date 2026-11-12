//! Host commands against a fake `docker` (and `gh`) on PATH: the real Docker is never called.

mod common;
use common::test_exe;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use zephyr::agent::AGENT_PATH;

const CONTAINER: &str = "zephyr-lanetest";
const GH_TOKEN: &str = "gho_FAKE_GH_TOKEN_0123";
const CLAUDE_TOKEN: &str = "sk-ant-oat01-FAKE-CLAUDE-TOKEN";

/// Records each call's argv (fields split by \x1f) in calls.log, and stdin of `exec -i` in stdin.log.
const DOCKER: &str = r#"#!/bin/bash
log="$SHIM_DIR/calls.log"
(IFS=$'\x1f'; echo "docker$IFS$*") >> "$log"
[ -n "$SHIM_DOCKER_DOWN" ] && exit 1
# `zephyr __file` in the container serves the daemon's files from $SHIM_DIR/files.
case " $* " in
  *" __file get "*) f="$SHIM_DIR/files/${@: -1}"; [ -f "$f" ] || exit 3; cat "$f"; exit 0 ;;
  *" __file put "*) [ -n "$SHIM_PUT_FAIL" ] && exit 1; mkdir -p "$SHIM_DIR/files"; cat > "$SHIM_DIR/files/${@: -1}"; exit 0 ;;
esac
[ -n "$GH_TOKEN" ] && echo "GH_TOKEN set" >> "$SHIM_DIR/env.log"
case "$1" in
  inspect) [ -n "$SHIM_NO_CONTAINER" ] && exit 1
    case "$*" in
      *NetworkMode*) echo "${SHIM_NETMODE:-$ZEPHYR_CONTAINER}" ;;
      # The container's environment: the image's, and what `up` passed when it made it.
      *Config.Env*) [ "$SHIM_NETMODE" = host ] || echo ZEPHYR_NETWORK=bridge; echo AGENT_BROWSER_ARGS=--no-sandbox ;;
      *IPAddress*) echo "${SHIM_IP:-172.18.0.2}" ;;
      *) echo "${SHIM_STATE:-running}" ;;
    esac; exit 0 ;;
  info) echo "${SHIM_RUNTIMES:-io.containerd.runc.v2 runc}"; exit 0 ;;
  # `docker network inspect NAME` fails when the network is not there yet.
  network) [ "$2" = inspect ] && exit "${SHIM_NO_NETWORK:-0}"; exit 0 ;;
  image) exit "${SHIM_IMAGE_MISSING:-0}" ;;
  pull) exit "${SHIM_PULL_FAIL:-0}" ;;
  # Reads the token as the real one does: exiting first would kill `gh auth token |` with SIGPIPE.
  login) cat > /dev/null; exit 0 ;;
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
      # The image's connector catalog (DESIGN §4.22), served from $SHIM_DIR/connectors.
      *" /usr/bin/cat /opt/zephyr/connectors/"*) f="$SHIM_DIR/connectors/$(basename "${@: -1}")"; [ -f "$f" ] || { echo "cat: no such file" >&2; exit 1; }; cat "$f" ;;
      *" /usr/bin/ls /opt/zephyr/connectors "*) ls "$SHIM_DIR/connectors" ;;
      *" --list-models "*) printf 'provider model context\nanthropic claude-sonnet-5 200K\nopenai-codex gpt-5.5 400K\nopenai-codex gpt-5.4 400K\n' ;;
      *" auth check "*) echo '{"ready":true,"provider":"claude-code","source":"auth.json"}' ;;
      *" zephyr status "*) echo "daemon: ${SHIM_DAEMON:-running}"; [ -n "$SHIM_STATUS" ] && printf '%b\n' "$SHIM_STATUS"; exit "${SHIM_STATUS_EXIT:-0}" ;;
      *" zephyr --version "*) [ -n "$SHIM_VERSION" ] || exit 2; echo "zephyr $SHIM_VERSION" ;;
      *" zephyr tui "*) exit "${SHIM_TUI_EXIT:-0}" ;;
      *" zephyr doctor "*) exit "${SHIM_DOCTOR_EXIT:-0}" ;;
      *" zephyr signal-setup "*) exit "${SHIM_SETUP_EXIT:-0}" ;;
      # `zephyr web enrol [NAME]` and `zephyr web`: the daemon's replies over the chat socket.
      *" zephyr web enrol"*) echo "[zephyr] open https://box.tail1234.ts.net/#enrol=ABCDE-FGHJK on the phone within 10 min, or type the code there; it works once" ;;
      *" zephyr web "*) echo "[zephyr] web app https://box.tail1234.ts.net · passkeys: ${SHIM_PASSKEYS:-none} · 0 sessions · push on 0 devices" ;;
      # `zephyr set KEY VALUE`: the daemon's reply, a refusal when SHIM_SET_REFUSE is set.
      *" zephyr set "*) [ -n "$SHIM_SET_DOWN" ] && { echo "zephyr: the daemon is not running" >&2; exit 3; }; [ -n "$SHIM_SET_REFUSE" ] && { echo "[zephyr] $SHIM_SET_REFUSE"; exit 1; }; echo "[zephyr] ${@: -2:1} 1 → ${@: -1}" ;;
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

/// `tailscale`: logged like `docker`. `status --json` prints `$SHIM_TS_STATUS` (unset: tailscale
/// is not logged in), `serve status` prints `$SHIM_TS_SERVE`, and `serve --bg` is refused for
/// want of operator rights when `$SHIM_TS_DENIED` is set.
const TAILSCALE: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "tailscale$IFS$*") >> "$SHIM_DIR/calls.log"
case "$1 $2" in
  "status --json") [ -n "$SHIM_TS_STATUS" ] || exit 1; echo "$SHIM_TS_STATUS" ;;
  "serve status") echo "$SHIM_TS_SERVE" ;;
  "serve --bg") [ -n "$SHIM_TS_DENIED" ] && { echo "Access denied: serve config denied" >&2; exit 1; } ;;
esac
exit 0
"#;

const CURL: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "curl$IFS$*") >> "$SHIM_DIR/calls.log"
exit "${SHIM_CURL_FAIL:-0}"
"#;

/// `cargo`: logged like `docker`. `install` writes a fake `~/.cargo/bin/zephyr` that logs how it
/// was run to exec.log, as the image's CLI does.
const CARGO: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "cargo$IFS$*") >> "$SHIM_DIR/calls.log"
case "$1" in
  --version) echo "cargo 1.95.0 (abc 2026-01-01)" ;;
  install) mkdir -p "$HOME/.cargo/bin"
    echo "ZEPHYR_BUILD_REPO=${ZEPHYR_BUILD_REPO-unset}" >> "$SHIM_DIR/cargo-env.log"
    printf '#!/bin/bash\necho "$@" >> "%s/exec.log"\n' "$SHIM_DIR" > "$HOME/.cargo/bin/zephyr"
    chmod 755 "$HOME/.cargo/bin/zephyr" ;;
esac
"#;

/// `sudo` and `wget`: logged and refused, for tests that check they are never called.
const REFUSED: &str = r#"#!/bin/bash
(IFS=$'\x1f'; echo "$(basename "$0")$IFS$*") >> "$SHIM_DIR/calls.log"
exit 1
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
            ("tailscale", TAILSCALE),
            ("cargo", CARGO),
            ("sudo", REFUSED),
            ("wget", REFUSED),
        ] {
            test_exe(&bin.join(name), body);
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
    /// `bash deploy/install.sh <args>`, with the same environment as the CLI.
    fn install(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let script = concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy/install.sh");
        let args: Vec<&str> = [script].into_iter().chain(args.iter().copied()).collect();
        self.run_input(Path::new("bash"), &args, env, None)
    }

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

/// Owned strings, to compare with a recorded call.
fn s(words: &[&str]) -> Vec<String> {
    words.iter().map(|w| w.to_string()).collect()
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
    // The image's own browser flags (the shim's) come first, then the proxy's.
    let proxy = zephyr::agent::proxy_env(Some("--no-sandbox".into())).into_iter();
    let proxy: Vec<String> = proxy.map(|(k, v)| format!("{k}={v}")).collect();
    assert!(proxy.contains(
        &"AGENT_BROWSER_ARGS=--no-sandbox,--proxy-server=http://127.0.0.1:7792".to_string()
    ));
    let proxy: Vec<&str> = proxy.iter().flat_map(|p| ["-e", p.as_str()]).collect();
    find_agent_with(calls, &proxy, rest)
}

/// A `docker exec` as the agent with `proxy` after its PATH: the network floor's variables
/// (DESIGN §4.20), or nothing under host networking.
fn find_agent_with<'a>(calls: &'a [Vec<String>], proxy: &[&str], rest: &[&str]) -> &'a Vec<String> {
    let ids = ids();
    let path = format!("PATH={AGENT_PATH}");
    let prefix: Vec<&str> = ["docker", "exec", "-u", &ids, "-w", "/zephyr", "-e", &path]
        .into_iter()
        .chain(proxy.iter().copied())
        .chain(rest.iter().copied())
        .collect();
    find(calls, &prefix)
}

/// Whether `call` has `flag` followed by `value`.
fn has_pair(call: &[String], flag: &str, value: &str) -> bool {
    call.windows(2).any(|w| w[0] == flag && w[1] == value)
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().mode() & 0o777
}

/// Answers a prompting `zephyr` on a pty, one `(prompt text, keys)` step at a time: each is sent
/// once the prompt's text has been printed. Returns the exit code and everything printed.
const PTY_DRIVER: &str = r#"
import json, os, pty, re, select, sys, time
steps = json.loads(sys.argv[1]); argv = sys.argv[2:]
pid, fd = pty.fork()
if pid == 0:
    os.execv(argv[0], argv)
seen, at, deadline = "", 0, time.time() + 60
while time.time() < deadline:
    r, _, _ = select.select([fd], [], [], 0.2)
    if r:
        try:
            data = os.read(fd, 4096)
        except OSError:
            break
        if not data:
            break
        seen += re.sub(r"\x1b\[[0-9;?]*[A-Za-z]", "", data.decode(errors="replace"))
    if steps:
        prompt, keys = steps[0]
        i = seen.find(prompt, at)
        if i >= 0:
            at = i + len(prompt)
            time.sleep(0.2)
            # The daemon's edits while the owner answers this prompt (`pty_edits`).
            for p, path, text in json.loads(os.environ.get("PTY_EDITS", "[]")):
                if p == prompt:
                    with open(path, "a") as f:
                        f.write(text)
            os.write(fd, keys.encode())
            steps.pop(0)
if time.time() >= deadline:
    os.kill(pid, 9)  # hung: fail the test, never wait on it
_, status = os.waitpid(pid, 0)
print(seen)
print("LEFT", len(steps))
sys.exit(os.waitstatus_to_exitcode(status))
"#;

impl Env {
    /// `zephyr <args>` on a terminal answering `steps` (see `PTY_DRIVER`): `(code, output)`.
    fn on_a_driven_tty(&self, args: &[&str], steps: &[(&str, &str)]) -> (i32, String) {
        self.on_a_driven_tty_env(args, steps, &[])
    }

    /// `on_a_driven_tty` with `env` set too.
    fn on_a_driven_tty_env(
        &self,
        args: &[&str],
        steps: &[(&str, &str)],
        env: &[(&str, &str)],
    ) -> (i32, String) {
        let path = format!(
            "{}:{}",
            self.dir.join("bin").display(),
            std::env::var("PATH").unwrap()
        );
        let steps = serde_json::to_string(steps).unwrap();
        let out = Command::new("python3")
            .args(["-I", "-c", PTY_DRIVER, &steps, env!("CARGO_BIN_EXE_zephyr")])
            .args(args)
            .env_clear()
            .env("PATH", path)
            .env("HOME", &self.home)
            .env("SHIM_DIR", &self.dir)
            .env("ZEPHYR_CONTAINER", CONTAINER)
            .env("ZEPHYR_IMAGE", "zephyr-lanetest")
            .env("TERM", "xterm")
            .envs(env.iter().copied())
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        (out.status.code().unwrap_or(-1), stdout(&out))
    }
}

#[test]
fn init_asks_for_a_preset_and_writes_it() {
    const PIN: &str = "4829105";
    const DOWN: &str = "\x1b[B";
    let env = Env::new("init-preset");
    let projects = env.projects.display().to_string();
    let (code, shown) = env.on_a_driven_tty(
        &["init", "--projects", &projects, "--no-start"],
        &[
            ("Providers", "\n"),
            ("Set up discord?", "n"),
            ("Set up imessage?", "n"),
            ("Set up telegram?", "n"),
            ("Set up signal?", "n"),
            // manual is first; semi is one down.
            ("Approvals: how much", &format!("{DOWN}\n")),
            ("Add private facts", "y"),
            ("Fact name", "pin\n"),
            ("Kind", &format!("{DOWN}\n")),
            ("value of pin (hidden)", &format!("{PIN}\n")),
            ("Add another fact?", "n"),
            (
                "Set up the web app (needs Tailscale with HTTPS certificates)?",
                "n",
            ),
        ],
    );
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    assert_eq!(code, 0, "{shown}");
    let config = env.config();
    assert!(
        config.contains("[approvals]") && config.contains("preset = \"semi\""),
        "{config}"
    );
    let facts = std::fs::read_to_string(env.daemon().join("facts.toml")).unwrap();
    assert!(facts.contains(PIN) && facts.contains("digits"), "{facts}");
    assert!(!shown.contains(PIN), "the value was echoed");
    assert!(!format!("{:?}", env.calls()).contains(PIN), "value in argv");
}

/// `--network` is the one CLI way to set host.toml's network (after the first `up` the file is
/// root's); without it a re-run keeps what is there.
#[test]
fn init_network_flag_sets_and_keeps_the_mode() {
    let env = Env::new("init-network");
    let host = || read(&env.daemon().join("host.toml"));
    let projects = env.projects.display().to_string();
    let init = |extra: &[&str]| {
        let mut args = vec![
            "init",
            "--non-interactive",
            "--no-start",
            "--projects",
            &projects,
        ];
        args.extend(extra);
        env.zephyr(&args)
    };
    let out = init(&["--network", "host"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(host().contains("network = \"host\""), "{}", host());
    assert!(stdout(&out)
        .contains("network: host (no network floor; zephyr init --network bridge turns it on)"));
    let out = init(&[]);
    assert!(out.status.success());
    assert!(host().contains("network = \"host\""), "kept: {}", host());
    assert!(stdout(&out).contains("network: host"));

    let before = host();
    let out = init(&["--network", "foo"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("--network must be bridge or host"), "{err}");
    assert_eq!(host(), before, "refused before anything was written");

    let out = init(&["--network", "bridge"]);
    assert!(out.status.success());
    assert!(host().contains("network = \"bridge\""), "{}", host());
    assert!(stdout(&out).contains("network: bridge (the agent's network floor)"));
}

/// What an owner's install holds: two accounts, Discord, iMessage, the web app and a preset.
fn configure_an_install(env: &Env) {
    env.init(&[
        "--no-start",
        "--account",
        "codex-a=openai-codex/gpt-5.5",
        "--account",
        "claude-a=anthropic/claude-sonnet-5",
    ]);
    env.ok(&["config", "set", "owners.discord", "[\"1234\"]"]);
    env.ok(&["config", "set", "discord.token_env", "ZEPHYR_DISCORD_TOKEN"]);
    env.ok(&["config", "set", "owners.imessage", "[\"+15550100\"]"]);
    let mut cfg = env.config();
    cfg.push_str(
        "\n[imessage]\nlisten = \"100.64.0.1:7791\"\nserver_url = \"http://100.64.0.2:1234\"\n\
         password_env = \"ZEPHYR_BB_PASSWORD\"\nsecret_env = \"ZEPHYR_BB_SECRET\"\n\
         \n[web]\nhost = \"box.tail1234.ts.net\"\nport = 7793\n\
         \n[approvals]\npreset = \"semi\"\n",
    );
    std::fs::write(env.daemon().join("zephyr.toml"), cfg).unwrap();
}

/// Review Focus 2: `init` again on the owner's install, Enter at every prompt, changes nothing:
/// the same files byte for byte, no `tailscale serve`, and one `docker restart` at the end.
#[test]
fn init_again_on_a_configured_install_changes_nothing() {
    let env = Env::new("init-rerun");
    configure_an_install(&env);
    // After the first `up` the daemon's files are root's, reached through the container.
    env.split();
    let files = env.dir.join("files");
    let snapshot = || ["zephyr.toml", "host.toml"].map(|f| std::fs::read(files.join(f)).unwrap());
    let before = snapshot();
    env.calls();
    let forwards =
        "https://box.tail1234.ts.net (tailnet only)\n|-- / proxy http://127.0.0.1:7793\n\n\
                    tcp://box.tail1234.ts.net:7791 (tailnet only)\n|--> tcp://127.0.0.1:7791";
    let (_, shown) = env.on_a_driven_tty_env(
        &["init"],
        &[
            ("Projects folder", "\n"),
            ("Providers", "\n"),
            ("Add another", "\n"),
            ("Add another", "\n"),
            ("Failover order", "\n"),
            ("Change the discord channel?", "\n"),
            ("Change the imessage channel?", "\n"),
            ("Set up telegram?", "\n"),
            ("Set up signal?", "\n"),
            ("Add private facts", "\n"),
            ("Change the web app?", "\n"),
        ],
        &[("SHIM_TS_SERVE", forwards)],
    );
    env.unsplit();
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    assert!(
        shown.contains("web app: https://box.tail1234.ts.net"),
        "{shown}"
    );
    assert!(!shown.contains("Forward this machine"), "{shown}");
    // The one failure is doctor's: the shim cannot make the daemon's dir root's.
    assert!(shown.contains("is root's and private"), "{shown}");
    let after = snapshot();
    for (i, name) in ["zephyr.toml", "host.toml"].iter().enumerate() {
        assert!(
            before[i] == after[i],
            "{name} changed:\n{}\n---\n{}",
            String::from_utf8_lossy(&before[i]),
            String::from_utf8_lossy(&after[i])
        );
    }
    let calls = env.calls();
    let count = |prefix: &[&str]| (calls.iter()).filter(|c| c.starts_with(&s(prefix))).count();
    assert_eq!(count(&["tailscale", "serve", "--bg"]), 0, "{calls:#?}");
    assert_eq!(count(&["docker", "restart"]), 1, "{calls:#?}");
    // The `up` init always starts with, and none after it.
    assert_eq!(count(RUN), 1, "{calls:#?}");
}

/// Interactive init reads zephyr.toml again after the failover order and the approvals
/// prompts: what the daemon wrote while the owner chose is kept.
#[test]
fn init_keeps_what_the_daemon_wrote_during_its_prompts() {
    let env = Env::new("init-prompt-edits");
    configure_an_install(&env);
    let toml = env.daemon().join("zephyr.toml");
    let text = env
        .config()
        .replace("\n[approvals]\npreset = \"semi\"\n", "\n");
    std::fs::write(&toml, text).unwrap();
    let edits = pty_edits(&[
        ("Failover order", &toml, "\n[tasks]\nparallel = 3\n"),
        (
            "Approvals: how much",
            &toml,
            "\n[budget]\ndaily_tokens = 1234\n",
        ),
    ]);
    let (_, shown) = env.on_a_driven_tty_env(
        &["init"],
        &[
            ("Projects folder", "\n"),
            ("Providers", "\n"),
            ("Add another", "\n"),
            ("Add another", "\n"),
            ("Failover order", "\n"),
            ("Change the discord channel?", "\n"),
            ("Change the imessage channel?", "\n"),
            ("Set up telegram?", "\n"),
            ("Set up signal?", "\n"),
            ("Approvals: how much", "\n"),
            ("Add private facts", "\n"),
            ("Change the web app?", "\n"),
        ],
        &[("PTY_EDITS", &edits)],
    );
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    let config = env.config();
    assert!(config.contains("parallel = 3"), "{config}");
    assert!(config.contains("daily_tokens = 1234"), "{config}");
    assert!(config.contains("preset = \"manual\""), "{config}");
}

/// A port init added (the iMessage webhook) is published by making the container again: a
/// restart cannot add one.
#[test]
fn init_adding_imessage_publishes_its_port_and_offers_the_forward() {
    let env = Env::new("init-imessage");
    env.init(&["--no-start"]);
    let mut steps = vec![
        ("Projects folder", "\n"),
        ("Providers", "\n"),
        ("Set up discord?", "n"),
        ("Set up imessage?", "y"),
    ];
    steps.extend(IMESSAGE_STEPS);
    steps.extend([
        (FORWARD_Q, "y"),
        ("Set up telegram?", "n"),
        ("Set up signal?", "n"),
        ("Approvals: how much", "\n"),
        ("Add private facts", "n"),
        ("Set up the web app", "n"),
    ]);
    let (code, shown) = env.on_a_driven_tty(&["init"], &steps);
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    assert_eq!(code, 0, "{shown}");
    let calls = env.calls();
    find(&calls, &FORWARD);
    let runs: Vec<&Vec<String>> = (calls.iter()).filter(|c| c.starts_with(&s(RUN))).collect();
    assert_eq!(runs.len(), 2, "{calls:#?}");
    assert!(
        has_pair(runs[1], "-p", "127.0.0.1:7791:7791"),
        "{:?}",
        runs[1]
    );
    assert!(
        !has_pair(runs[0], "-p", "127.0.0.1:7791:7791"),
        "{:?}",
        runs[0]
    );
    assert_eq!(at(&calls, &["docker", "restart"], ""), None, "{calls:#?}");
}

/// init's prompts up to web setup's restart question, for a fresh install that sets up the web app.
fn init_into_web_setup(restart: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![
        ("Projects folder", "\n"),
        ("Providers", "\n"),
        ("Set up discord?", "n"),
        ("Set up imessage?", "n"),
        ("Set up telegram?", "n"),
        ("Set up signal?", "n"),
        ("Approvals: how much", "\n"),
        ("Add private facts", "n"),
        ("Set up the web app", "y"),
        ("Write [web] host", "y"),
        ("Run `tailscale serve", "y"),
        ("Start zephyr again", restart),
    ]
}

/// The owner declined web setup's restart: init neither makes the container again nor restarts it.
#[test]
fn init_leaves_the_container_alone_when_the_web_restart_is_declined() {
    let env = Env::new("init-web-declined");
    env.init(&["--no-start"]);
    let steps = init_into_web_setup("n");
    let (code, shown) =
        env.on_a_driven_tty_env(&["init"], &steps, &[("SHIM_TS_STATUS", TS_STATUS)]);
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    assert_eq!(code, 0, "{shown}");
    let calls = env.calls();
    let runs = calls.iter().filter(|c| c.starts_with(&s(RUN))).count();
    assert_eq!(runs, 1, "only init's first up: {calls:#?}");
    assert_eq!(at(&calls, &["docker", "restart"], ""), None, "{calls:#?}");
    assert!(shown.contains("zephyr up applies the changes"), "{shown}");
}

/// web setup started the container, then failed: init prints the error and starts nothing again.
#[test]
fn init_does_not_restart_again_after_web_setup_fails_past_its_restart() {
    let env = Env::new("init-web-error");
    env.init(&["--no-start"]);
    let path = env.daemon().join("zephyr.toml");
    let text = format!(
        "{}\n[settings]\nstrong_channels = [\"tui\", \"chat\"]\n",
        env.config()
    );
    std::fs::write(&path, text).unwrap();
    let mut steps = init_into_web_setup("y");
    steps.push(("Let an approval signed with a passkey", "y"));
    let vars = [("SHIM_TS_STATUS", TS_STATUS), ("SHIM_SET_REFUSE", "no")];
    let (code, shown) = env.on_a_driven_tty_env(&["init"], &steps, &vars);
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    assert_eq!(code, 0, "{shown}");
    assert!(shown.contains("web app: "), "the error is shown: {shown}");
    let calls = env.calls();
    let runs = calls.iter().filter(|c| c.starts_with(&s(RUN))).count();
    assert_eq!(runs, 2, "init's first up and web setup's: {calls:#?}");
    assert_eq!(at(&calls, &["docker", "restart"], ""), None, "{calls:#?}");
}

/// Under host networking the web app cannot run (DESIGN §4.3.3), so init does not offer it.
#[test]
fn init_under_host_networking_skips_the_web_app() {
    let env = Env::new("init-host-web");
    let projects = env.projects.display().to_string();
    let (code, shown) = env.on_a_driven_tty(
        &["init", "--network", "host", "--projects", &projects],
        &[
            ("Providers", "\n"),
            ("Set up discord?", "n"),
            ("Set up imessage?", "n"),
            ("Set up telegram?", "n"),
            ("Set up signal?", "n"),
            ("Approvals: how much", "\n"),
            ("Add private facts", "n"),
        ],
    );
    assert!(
        shown.contains("LEFT 0"),
        "every prompt was answered: {shown}"
    );
    assert_eq!(code, 0, "{shown}");
    assert!(!shown.contains("Set up the web app"), "{shown}");
    assert!(
        shown.contains("web app: the web app needs the network floor"),
        "{shown}"
    );
}

#[test]
fn non_interactive_init_hints_at_channels_and_the_web_app() {
    const CHANNEL: &str = "add a channel: zephyr channel add discord|imessage";
    const WEB: &str = "web app: zephyr web setup";
    let env = Env::new("init-hints");
    let projects = env.projects.display().to_string();
    let args = [
        "init",
        "--non-interactive",
        "--no-start",
        "--projects",
        &projects,
    ];
    let text = env.ok(&args);
    assert!(text.contains(CHANNEL) && text.contains(WEB), "{text}");
    configure_an_install(&env);
    let text = env.ok(&args);
    assert!(!text.contains(CHANNEL) && !text.contains(WEB), "{text}");
    // Under host networking there is no web app to set up.
    let host = Env::new("init-hints-host");
    let projects = host.projects.display().to_string();
    let mut args = args.to_vec();
    args[4] = &projects;
    args.extend(["--network", "host"]);
    let text = host.ok(&args);
    assert!(text.contains(CHANNEL) && !text.contains(WEB), "{text}");
}

/// The owner's comments on host.toml's lines survive init writing them again.
#[test]
fn init_keeps_the_comments_on_the_lines_it_writes() {
    let env = Env::new("init-decor");
    env.init(&["--no-start"]);
    let path = env.daemon().join("host.toml");
    let projects = env.projects.canonicalize().unwrap().display().to_string();
    std::fs::write(
        &path,
        format!("projects = {projects:?} # where I work\nnetwork = \"host\" # for now\n"),
    )
    .unwrap();
    env.init(&["--no-start", "--network", "bridge"]);
    let text = read(&path);
    assert!(
        text.contains(&format!("projects = {projects:?} # where I work\n")),
        "{text}"
    );
    assert!(text.contains("network = \"bridge\" # for now\n"), "{text}");
}

#[test]
fn init_non_interactive_takes_preset_flag() {
    let env = Env::new("init-preset-flag");
    env.init(&["--no-start", "--preset", "full"]);
    let config = env.config();
    assert!(
        config.contains("[approvals]") && config.contains("preset = \"full\""),
        "{config}"
    );
    // Not given: the table stays absent, which is manual; and no facts are asked for.
    let plain = Env::new("init-no-preset");
    plain.init(&["--no-start"]);
    assert!(!plain.config().contains("approvals"));
    assert!(!plain.daemon().join("facts.toml").exists());
    // A word that is no preset is refused before anything is written.
    let bad = Env::new("init-bad-preset");
    let projects = bad.projects.display().to_string();
    let out = bad.zephyr(&[
        "init",
        "--non-interactive",
        "--projects",
        &projects,
        "--preset",
        "some",
    ]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("manual, semi or full"));
    assert!(!bad.daemon().join("zephyr.toml").exists());
}

/// D11: a re-run of `init --preset` writes the preset as `/approvals PRESET` does
/// (`preset_edits`): a hand-set class looser than the new preset is cleared, a stricter one kept.
#[test]
fn init_again_with_a_preset_drops_looser_hand_set_classes() {
    let env = Env::new("init-preset-edits");
    env.init(&["--no-start", "--preset", "full"]);
    let path = env.daemon().join("zephyr.toml");
    let text = env.config().replace(
        "[approvals]\npreset = \"full\"",
        "[approvals]\npreset = \"full\"\nmessages = \"auto\"\nposts = \"ask\"\nmoney = \"auto 50\"",
    );
    assert!(text.contains("messages = \"auto\""), "{text}");
    std::fs::write(&path, text).unwrap();
    env.init(&["--no-start", "--preset", "manual"]);
    let config = env.config();
    assert!(config.contains("preset = \"manual\""), "{config}");
    assert!(
        !config.contains("messages ="),
        "looser than manual: {config}"
    );
    assert!(!config.contains("money ="), "looser than manual: {config}");
    assert!(config.contains("posts = \"ask\""), "as strict: {config}");
}

/// `mcp add` with a secret asked at a prompt.
const MCP_ADD: [&str; 7] = [
    "mcp",
    "add",
    "mail",
    "--secret",
    "MAIL_KEY",
    "--",
    "mail-server",
];

/// `PTY_EDITS` for `on_a_driven_tty_env`: at each prompt, `text` appended to `path`.
fn pty_edits(edits: &[(&str, &Path, &str)]) -> String {
    let edits: Vec<(&str, String, &str)> = (edits.iter())
        .map(|(p, path, text)| (*p, path.display().to_string(), *text))
        .collect();
    serde_json::to_string(&edits).unwrap()
}

/// Ctrl-C at a secret's prompt leaves every file as it was: nothing is written until every
/// answer is in.
#[test]
fn a_cancelled_secret_prompt_saves_nothing() {
    let env = Env::new("prompt-cancel");
    env.init(&["--no-start"]);
    let read = |name: &str| std::fs::read_to_string(env.daemon().join(name)).ok();
    let before = (read("zephyr.toml"), read("secrets.env"));
    let (code, out) = env.on_a_driven_tty(&MCP_ADD, &[("MAIL_KEY (hidden)", "\x03")]);
    assert_ne!(code, 0, "{out}");
    assert_eq!((read("zephyr.toml"), read("secrets.env")), before, "{out}");
}

/// The secret is saved before the connector that uses it: the daemon never reloads a
/// zephyr.toml naming a secret secrets.env does not hold yet, or a new command with an old value.
#[test]
fn mcp_add_saves_the_secret_before_the_connector() {
    let env = Env::new("prompt-order");
    env.init(&["--no-start"]);
    env.split();
    env.calls();
    let (code, out) = env.on_a_driven_tty(&MCP_ADD, &[("MAIL_KEY (hidden)", "s3cret\r")]);
    env.unsplit();
    assert_eq!(code, 0, "{out}");
    let puts: Vec<String> = (env.calls().into_iter())
        .filter(|c| c.contains(&"__file".to_string()) && c.contains(&"put".to_string()))
        .map(|c| c.last().unwrap().clone())
        .collect();
    assert_eq!(puts, ["secrets.env", "zephyr.toml"], "{out}");
}

/// The daemon may write zephyr.toml or vault.toml while the owner types a secret: what it
/// wrote meanwhile is kept, not undone by a copy read before the prompt.
#[test]
fn an_edit_made_while_a_secret_is_typed_survives() {
    let env = Env::new("prompt-edit");
    env.init(&["--no-start"]);
    let toml = env.daemon().join("zephyr.toml");
    let edits = pty_edits(&[("MAIL_KEY (hidden)", &toml, "\n[tasks]\nparallel = 3\n")]);
    let (code, out) = env.on_a_driven_tty_env(
        &MCP_ADD,
        &[("MAIL_KEY (hidden)", "s3cret\r")],
        &[("PTY_EDITS", &edits)],
    );
    assert_eq!(code, 0, "{out}");
    let config = env.config();
    assert!(config.contains("parallel = 3"), "{config}");
    assert!(config.contains("\"mail-server\""), "{config}");

    let vault = env.daemon().join("vault.toml");
    let token = "[[token]]\nname = \"GH_TOKEN\"\nvalue = \"ghp_x\"\n";
    let edits = pty_edits(&[("password for github (hidden)", &vault, token)]);
    let (code, out) = env.on_a_driven_tty_env(
        &[
            "vault",
            "add",
            "login",
            "github",
            "--site",
            "https://github.com",
        ],
        &[("password for github (hidden)", "hunter22\r")],
        &[("PTY_EDITS", &edits)],
    );
    assert_eq!(code, 0, "{out}");
    let list = env.ok(&["vault", "list"]);
    assert!(
        list.contains("token GH_TOKEN") && list.contains("login github"),
        "{list}"
    );
}

/// D4: adding a connector again (a new URL or command) keeps the classes the owner gave its
/// tools.
#[test]
fn mcp_add_again_keeps_the_owners_classes() {
    let env = Env::new("mcp-classes");
    env.init(&["--no-start"]);
    env.ok(&["mcp", "add", "mail", "--", "mail-server", "--stdio"]);
    let path = env.daemon().join("zephyr.toml");
    let text = env.config().replace(
        "name = \"mail\"",
        "name = \"mail\"\nclasses = { archive = \"posts\", note = \"none\" }",
    );
    assert!(text.contains("archive"), "{text}");
    std::fs::write(&path, text).unwrap();
    env.ok(&[
        "mcp",
        "add",
        "mail",
        "--url",
        "https://mail.example.com/mcp",
    ]);
    let config = env.config();
    assert!(config.contains("https://mail.example.com/mcp"), "{config}");
    assert!(!config.contains("mail-server"), "{config}");
    assert!(
        config.contains("archive = \"posts\"") && config.contains("note = \"none\""),
        "{config}"
    );
    env.ok(&["mcp", "add", "mail", "--", "other-server"]);
    let config = env.config();
    assert!(
        config.contains("other-server") && config.contains("archive = \"posts\""),
        "{config}"
    );
    // A new connector has none.
    env.ok(&["mcp", "add", "docs", "--", "docs-server"]);
    assert_eq!(
        env.config().matches("classes").count(),
        1,
        "{}",
        env.config()
    );
}

/// DESIGN §4.21: where a connector's secrets go is set where the owner adds it, and kept when
/// it is added again.
#[test]
fn mcp_add_takes_hosts_and_plain_and_keeps_them() {
    let env = Env::new("mcp-hosts");
    env.init(&["--no-start"]);
    let add = ["mcp", "add", "mail", "--host", "api.mail.test"];
    env.ok(&[&add[..], &["--host", "mail.test:8443", "--", "mail-server"]].concat());
    let config = env.config();
    assert!(
        config.contains("hosts = \"api.mail.test, mail.test:8443\""),
        "{config}"
    );
    env.ok(&["mcp", "add", "mail", "--plain", "--", "other-server"]);
    let config = env.config();
    assert!(
        config.contains("api.mail.test") && config.contains("secrets = \"plain\""),
        "{config}"
    );
    env.ok(&["mcp", "add", "mail", "--", "third-server"]);
    assert!(env.config().contains("secrets = \"plain\""));
    // A remote connector can only hold its real secret, where the agent can read it: adding
    // one with a secret says so and asks for `--plain`; without a secret there is nothing to say.
    let remote = [
        "mcp",
        "add",
        "docs",
        "--url",
        "https://docs.example.com/mcp",
    ];
    let header = ["--header", "Authorization=Bearer ${DOCS_TOKEN}"];
    let with_secret = [&remote[..], &header[..]].concat();
    let out = env.zephyr(&with_secret);
    assert!(!out.status.success());
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(said.contains("add --plain to accept that"), "{said}");
    assert!(!env.config().contains("docs.example.com"));
    env.ok(&[&with_secret[..], &["--plain"]].concat());
    let config = env.config();
    let docs = config.split("[[mcp]]").find(|t| t.contains("\"docs\""));
    assert!(docs.unwrap().contains("secrets = \"plain\""), "{config}");
    env.ok(&[
        "mcp",
        "add",
        "open",
        "--url",
        "https://open.example.com/mcp",
    ]);
    // A wildcard is refused and nothing is written.
    let before = env.config();
    let out = env.zephyr(&["mcp", "add", "mail", "--host", "*.mail.test", "--", "x"]);
    assert!(!out.status.success());
    assert_eq!(env.config(), before);
}

/// The image's catalog, as the fake container serves it.
fn with_catalog(env: &Env) {
    let to = env.dir.join("connectors");
    std::fs::create_dir_all(&to).unwrap();
    let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../deploy/connectors");
    for e in std::fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        std::fs::copy(e.path(), to.join(e.file_name())).unwrap();
    }
}

/// DESIGN §4.22: a catalog entry becomes an ordinary `[[mcp]]` table, read from the image's copy,
/// and adding it again keeps where the owner sends its secret and the classes they gave.
#[test]
fn mcp_add_from_the_catalog_writes_the_entry_and_keeps_the_owners_choices() {
    let env = Env::new("mcp-catalog");
    env.init(&["--no-start"]);
    with_catalog(&env);
    let out = env.ok(&["mcp", "add", "--catalog", "github"]);
    let config = env.config();
    assert!(
        config.contains("/usr/local/bin/github-mcp-server"),
        "{config}"
    );
    assert!(
        config.contains("GITHUB_PERSONAL_ACCESS_TOKEN = \"${GITHUB_TOKEN}\""),
        "{config}"
    );
    assert!(
        config.contains("hosts = \"api.github.com, raw.githubusercontent.com\""),
        "{config}"
    );
    assert!(
        config.contains("merge_pull_request = \"posts\""),
        "{config}"
    );
    assert!(!config.contains("secrets ="), "{config}");
    let said = out;
    assert_eq!(
        said.matches("zephyr mcp add github --secret GITHUB_TOKEN")
            .count(),
        1,
        "{said}"
    );
    // The owner widens the hosts and lets a merge through; secrets.env gets the token.
    let path = env.daemon().join("zephyr.toml");
    let edited = config
        .replace(
            "hosts = \"api.github.com, raw.githubusercontent.com\"",
            "hosts = \"api.github.com, evil.test\"",
        )
        .replace(
            "merge_pull_request = \"posts\"",
            "merge_pull_request = \"none\"",
        );
    std::fs::write(&path, edited).unwrap();
    let secrets = env.daemon().join("secrets.env");
    let mut text = std::fs::read_to_string(&secrets).unwrap_or_default();
    text.push_str("GITHUB_TOKEN=x\n");
    std::fs::write(&secrets, text).unwrap();
    let out = env.ok(&["mcp", "add", "--catalog", "github"]);
    let config = env.config();
    // The class stays the owner's; the hosts are the entry's again, never an old table's.
    assert!(
        config.contains("hosts = \"api.github.com, raw.githubusercontent.com\""),
        "{config}"
    );
    assert!(!config.contains("evil.test"), "{config}");
    assert!(config.contains("merge_pull_request = \"none\""), "{config}");
    assert!(config.contains("issue_write = \"posts\""), "{config}");
    assert_eq!(config.matches("[[mcp]]").count(), 1, "{config}");
    // Recorded as the catalog's, for doctor's note, in the daemon's own file (review of #53, 11).
    let added =
        || std::fs::read_to_string(env.daemon().join("catalog-added.json")).unwrap_or_default();
    assert_eq!(added(), "[\"github\"]");
    assert!(!config.contains("catalog"), "{config}");
    assert!(!out.contains("GITHUB_TOKEN"), "{out}");
    // An entry the image does not have, and a name that is no file name, are refused.
    for name in ["nope", "../etc/passwd"] {
        let out = env.zephyr(&["mcp", "add", "--catalog", name]);
        assert!(!out.status.success(), "{name}");
    }
    // Written again by hand, it is the owner's own.
    env.ok(&["mcp", "add", "github", "--", "my-github-server"]);
    assert_eq!(added(), "[]");
}

/// DESIGN §4.22: an entry that can only hold its real key says why, and is written only once
/// the owner accepts it with `--plain`.
#[test]
fn catalog_entries_that_hold_their_key_in_plain_need_plain() {
    let env = Env::new("mcp-catalog-plain");
    env.init(&["--no-start"]);
    with_catalog(&env);
    let refused = |args: &[&str]| {
        let before = env.config();
        let out = env.zephyr(args);
        assert!(!out.status.success(), "{args:?}");
        let err = String::from_utf8_lossy(&out.stderr).to_string();
        assert!(err.contains("add --plain"), "{err}");
        assert_eq!(env.config(), before);
    };
    refused(&["mcp", "add", "--catalog", "brave-search"]);
    let ha = ["mcp", "add", "--catalog", "home-assistant"];
    let lan = [&ha[..], &["--url", "http://ha.lan:8123/api/mcp"]].concat();
    refused(&lan);
    // Without a URL it cannot be written at all.
    assert!(!env.zephyr(&ha).status.success());
    env.ok(&["mcp", "add", "--catalog", "brave-search", "--plain"]);
    env.ok(&[&lan[..], &["--plain"]].concat());
    let config = env.config();
    assert_eq!(config.matches("secrets = \"plain\"").count(), 2, "{config}");
    assert!(config.contains("http://ha.lan:8123/api/mcp"), "{config}");
    assert!(config.contains("hosts = \"ha.lan:8123\""), "{config}");
    // A `plain` written before is no consent for this add (review of #53).
    refused(&["mcp", "add", "--catalog", "brave-search"]);
    // Over HTTPS the stand-in works: no `--plain` needed, and the old `plain` goes, said.
    let out = env.ok(&[&ha[..], &["--url", "https://ha.example.com/api/mcp"]].concat());
    assert!(out.contains("now it holds a stand-in"), "{out}");
    let config = env.config();
    let ha = config
        .split("[[mcp]]")
        .find(|t| t.contains("home-assistant"))
        .unwrap();
    assert!(
        !ha.contains("secrets") && ha.contains("hosts = \"ha.example.com\""),
        "{config}"
    );
}

#[test]
fn mcp_catalog_lists_the_entries_and_needs_the_container() {
    let env = Env::new("mcp-catalog-list");
    env.init(&["--no-start"]);
    with_catalog(&env);
    let out = env.ok(&["mcp", "catalog"]);
    let lines: Vec<String> = out.lines().map(String::from).collect();
    let names: Vec<&str> = lines.iter().filter_map(|l| l.split(':').next()).collect();
    assert_eq!(
        names,
        [
            "brave-search",
            "context7",
            "github",
            "home-assistant",
            "linear",
            "notion"
        ]
    );
    let out = env.zephyr_env(
        &["mcp", "add", "--catalog", "context7"],
        &[("SHIM_STATE", "exited")],
    );
    assert!(!out.status.success());
    assert!(!env.config().contains("context7"));
}

#[test]
fn init_writes_the_parsed_preset_name() {
    let env = Env::new("init-preset-trim");
    env.init(&["--no-start", "--preset", " Full"]);
    let config = env.config();
    assert!(config.contains("preset = \"full\""), "{config}");
}

/// D12: what `zephyr facts add` says follows the facts mode: held under `hold`, gone with a
/// notice under `warn`, not looked for under `off`.
#[test]
fn facts_add_says_what_the_facts_mode_does() {
    let env = Env::new("facts-mode-words");
    env.init(&["--no-start", "--preset", "manual"]);
    let add = |name: &str| {
        let out = env.zephyr_input(&["facts", "add", name, "--kind", "digits"], "4829105\n");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        stdout(&out)
    };
    assert!(add("a").contains("calls holding it are held for your OK"));
    let path = env.daemon().join("zephyr.toml");
    let full = env
        .config()
        .replace("preset = \"manual\"", "preset = \"full\"");
    std::fs::write(&path, &full).unwrap();
    let warn = add("b");
    assert!(
        warn.contains("calls holding it go, and you are told"),
        "{warn}"
    );
    assert!(!warn.contains("held for your OK"), "{warn}");
    std::fs::write(
        &path,
        full.replace("preset = \"full\"", "preset = \"full\"\nfacts = \"off\""),
    )
    .unwrap();
    let off = add("c");
    assert!(
        off.contains("not looked for while approvals.facts is off"),
        "{off}"
    );
}

#[test]
fn saving_a_fact_under_a_name_with_spaces_replaces_it_and_keeps_its_targets() {
    let env = Env::new("facts-trim");
    env.init(&["--no-start"]);
    let out = env.zephyr_input(&["facts", "add", "pin", "--kind", "digits"], "4829105\n");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    env.ok(&["facts", "allow", "pin", "mum@example.com"]);
    let out = env.zephyr_input(
        &["facts", "add", "pin ", "--kind", "digits"],
        "5550102345\n",
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        stdout(&out).contains("replaced the value of pin"),
        "{}",
        stdout(&out)
    );
    let list = env.ok(&["facts", "list"]);
    assert!(
        list.contains("pin (digits) allow: mum@example.com"),
        "{list}"
    );
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
        // The network floor (DESIGN §4.20): its own bridge, the capabilities `supervise` loads
        // the rules with and then drops, and only zephyr's port published, on loopback.
        ["--network", CONTAINER],
        ["--hostname", "zephyr"],
        ["--cap-add", "NET_ADMIN"],
        ["--cap-add", "SETPCAP"],
        ["-e", "ZEPHYR_NETWORK=bridge"],
        // What it publishes, for the daemon to compare its ports with.
        ["-e", "ZEPHYR_PUBLISHED=7790"],
        ["-p", "127.0.0.1:7790:7790"],
    ] {
        assert!(run.windows(2).any(|w| w == pair), "{pair:?} in {run:?}");
    }
    assert!(!has_pair(run, "--network", "host"), "{run:?}");
    let host = std::fs::read_to_string(daemon.join("host.toml")).unwrap();
    assert!(host.contains("network = \"bridge\""), "{host}");
    assert!(
        host.contains("# \"host\" runs without the network floor (DESIGN §4.20)"),
        "{host}"
    );
    // The network was there (the shim's `network inspect` succeeds), so none is created.
    find(&calls, &["docker", "network", "inspect", CONTAINER]);
    assert!(
        !calls
            .iter()
            .any(|c| c.len() > 2 && c[1] == "network" && c[2] == "create"),
        "{calls:#?}"
    );
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
    // The image's defaults, the full provider set: no build arg moves them.
    assert!(!build.iter().any(|a| a == "--build-arg"), "{build:?}");

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

/// This user's GitHub token goes only to ghcr.io: any other registry an image names (or Docker
/// Hub, which `user/zephyr` is) would be handed it by `docker login`.
#[test]
fn pull_logs_in_with_the_gh_token_only_at_ghcr() {
    let env = Env::new("pull-registry");
    env.init(&["--no-start"]);
    let failing = [
        ("SHIM_IMAGE_MISSING", "1"),
        ("SHIM_PULL_FAIL", "1"),
        ("SHIM_BUILD_FAIL", "1"),
    ];
    let logins = |calls: &[Vec<String>]| {
        calls
            .iter()
            .filter(|c| {
                c.starts_with(&s(&["docker", "login"])) || c.starts_with(&s(&["gh", "api", "user"]))
            })
            .count()
    };
    for image in [
        "registry.example.com/me/zephyr:1",
        "user/zephyr:1",
        "ghcr.io.evil.com/x:1",
        "localhost:5000/z:1",
    ] {
        let mut vars = failing.to_vec();
        vars.push(("ZEPHYR_IMAGE", image));
        let out = env.zephyr_env(&["up"], &vars);
        assert!(!out.status.success(), "{image}");
        let calls = env.calls();
        find(&calls, &["docker", "pull", image]);
        assert_eq!(logins(&calls), 0, "{image}: {calls:#?}");
        assert!(
            stdout(&out).contains(&format!("{image} is not on ghcr.io: docker login")),
            "{}",
            stdout(&out)
        );
    }
    let image = "ghcr.io/someone/zephyr:1";
    let mut vars = failing.to_vec();
    vars.push(("ZEPHYR_IMAGE", image));
    env.zephyr_env(&["up"], &vars);
    let calls = env.calls();
    let login = find(&calls, &["docker", "login", "ghcr.io"]);
    assert!(has_pair(login, "-u", "tester"), "{login:?}");
    assert!(login.contains(&"--password-stdin".to_string()), "{login:?}");
    let pulls = calls
        .iter()
        .filter(|c| c.starts_with(&s(&["docker", "pull", image])));
    assert_eq!(pulls.count(), 2, "pulled again after the login: {calls:#?}");
    assert!(!format!("{calls:?}").contains(GH_TOKEN));
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
    // In the owner's home, readable with Docker down.
    let saved = std::fs::read_to_string(env.home.join(".config/zephyr/alert-url")).unwrap();
    assert_eq!(saved.trim(), "https://ntfy.sh/zephyr-lane_1");
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
    // Failed replies are a count, posted when it grows, never a lasting "down".
    let failed = [(
        "SHIM_STATUS",
        "outbox  sent        4\noutbox  failed      2",
    )];
    check(&failed);
    let p = posts(&env);
    assert_eq!(p.len(), 1, "{p:?}");
    assert!(
        p[0].contains(&"2 more replies failed to send (2 in all)".to_string()),
        "{p:?}"
    );
    check(&failed);
    assert!(posts(&env).is_empty(), "the same count again: nothing new");
    check(&[]);
    assert!(posts(&env).is_empty(), "healthy all along");
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
        login[login.len() - 2]
            .starts_with("rpi auth login --provider claude-code --extension /opt/zephyr/ext/"),
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
    let said = env.ok(&["login", "openai-codex", "--no-verify"]);
    // Under the network floor the provider's callback cannot reach the container.
    assert!(
        said.starts_with("if the browser then cannot open 127.0.0.1, paste its address here\n"),
        "{said}"
    );
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

/// What `init` and `login` are given is checked before anything is written or signed in: a
/// model not PROVIDER/MODEL, a provider zephyr does not know. An account already in the file
/// keeps its name, and an error never repeats a pasted value.
#[test]
fn init_and_login_check_what_they_are_given_first() {
    let env = Env::new("check-first");
    env.init(&["--no-start"]);
    env.calls();
    let out = env.zephyr(&["login", "anthropic", "--model", "sonnet"]);
    assert!(!out.status.success(), "{out:?}");
    let accounts = std::fs::read_dir(env.data().join("accounts")).map_or(0, |d| d.count());
    assert_eq!(accounts, 0, "no account folder");
    assert!(
        !env.calls().iter().any(|c| c.contains(&"auth".to_string())),
        "no sign-in"
    );

    let host = || std::fs::read_to_string(env.daemon().join("host.toml")).unwrap();
    let before = (host(), env.config());
    let projects = env.dir.join("elsewhere");
    std::fs::create_dir_all(&projects).unwrap();
    let projects = projects.display().to_string();
    let init = |account: &str| {
        env.zephyr(&[
            "init",
            "--non-interactive",
            "--no-start",
            "--projects",
            &projects,
            "--network",
            "host",
            "--account",
            account,
        ])
    };
    for account in [
        "a=nomodel",
        "a=nope/model",
        CLAUDE_TOKEN,
        &format!("{CLAUDE_TOKEN}=claude-code/claude-sonnet-5"),
    ] {
        let out = init(account);
        assert!(!out.status.success(), "{account}: {out:?}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(!said.contains(CLAUDE_TOKEN), "{said}");
        assert_eq!((host(), env.config()), before, "{account}: nothing written");
    }

    // A name from before the daemon's rule (as `/login` names one today) is still that account's.
    let text = env.config().replace(
        "[owners]",
        "[[account]]\nname = \"claude_max.2\"\nmodel = \"claude-code/claude-sonnet-5\"\n\n[owners]",
    );
    std::fs::write(env.daemon().join("zephyr.toml"), text).unwrap();
    let out = init("claude_max.2=claude-code/claude-opus-5");
    assert!(out.status.success(), "{out:?}");
    assert!(
        env.config().contains("claude-code/claude-opus-5"),
        "{}",
        env.config()
    );
}

/// An account name the daemon would refuse is refused before anything is signed in or written.
#[test]
fn login_and_init_refuse_a_bad_account_name_and_write_nothing() {
    let env = Env::new("login-bad-name");
    env.init(&["--no-start"]);
    let before = env.config();
    env.calls();
    let out = env.zephyr_env(
        &[
            "login",
            "claude-code",
            "--account",
            "work@home",
            "--model",
            "claude-code/claude-sonnet-5",
        ],
        &[("CLAUDE_CODE_OAUTH_TOKEN", CLAUDE_TOKEN)],
    );
    assert!(!out.status.success(), "{out:?}");
    assert!(!env.data().join("accounts/work@home").exists());
    assert!(!env
        .calls()
        .iter()
        .any(|c| c.contains(&"script".to_string())));
    let projects = env.projects.display().to_string();
    let out = env.zephyr(&[
        "init",
        "--non-interactive",
        "--no-start",
        "--projects",
        &projects,
        "--account",
        "work@home=claude-code/claude-sonnet-5",
    ]);
    assert!(!out.status.success(), "{out:?}");
    assert_eq!(env.config(), before);
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
fn config_set_of_a_known_key_goes_through_the_daemon() {
    let env = Env::new("config-known");
    env.init(&["--no-start"]);
    let before = env.config();
    env.calls();
    let out = env.zephyr(&["config", "set", "budget.daily_tokens", "5000"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).contains("budget.daily_tokens 1 → 5000"),
        "{out:?}"
    );
    let calls = env.calls();
    find(
        &calls,
        &[
            "docker",
            "exec",
            CONTAINER,
            "zephyr",
            "set",
            "budget.daily_tokens",
            "5000",
        ],
    );
    assert!(
        !calls.iter().any(|c| c.contains(&"reload".to_string())),
        "the daemon applies it itself"
    );
    assert_eq!(env.config(), before, "the host does not edit the file");
    // The daemon's refusal is the command's failure, and its words reach the owner.
    let out = env.zephyr_env(
        &["config", "set", "budget.daily_tokens", "0"],
        &[("SHIM_SET_REFUSE", "budget.daily_tokens must be positive")],
    );
    assert!(!out.status.success());
    assert!(stdout(&out).contains("must be positive"), "{out:?}");
}

#[test]
fn config_set_with_the_container_up_but_the_daemon_down_edits_the_file() {
    let env = Env::new("config-down");
    env.init(&["--no-start"]);
    env.calls();
    let out = env.zephyr_env(
        &["config", "set", "budget.daily_tokens", "8000"],
        &[("SHIM_SET_DOWN", "1")],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).contains("applies when zephyr starts"),
        "{out:?}"
    );
    let calls = env.calls();
    find(&calls, &["docker", "exec", CONTAINER, "zephyr", "set"]);
    assert!(!calls.iter().any(|c| c.contains(&"reload".to_string())));
    assert_eq!(
        env.ok(&["config", "get", "budget.daily_tokens"]).trim(),
        "8000"
    );
}

#[test]
fn config_set_of_an_unknown_key_warns_and_saves() {
    let env = Env::new("config-unknown");
    env.init(&["--no-start"]);
    env.calls();
    let out = env.zephyr(&["config", "set", "attach.port", "7791"]);
    assert!(out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr)
            .contains("warn: attach.port is not a known setting; saved without checking"),
        "{out:?}"
    );
    assert_eq!(env.ok(&["config", "get", "attach.port"]).trim(), "7791");
    assert!(!env
        .calls()
        .iter()
        .any(|c| c.contains(&"set".to_string()) && c.contains(&"zephyr".to_string())));
}

#[test]
fn config_set_while_stopped_edits_the_file_as_before() {
    let env = Env::new("config-stopped");
    env.init(&["--no-start"]);
    env.calls();
    let out = env.zephyr_env(
        &["config", "set", "budget.daily_tokens", "7000"],
        &[("SHIM_STATE", "exited")],
    );
    assert!(out.status.success(), "{out:?}");
    assert!(
        stdout(&out).contains("applies when zephyr starts"),
        "{out:?}"
    );
    assert!(!String::from_utf8_lossy(&out.stderr).contains("warn:"));
    assert_eq!(
        env.ok(&["config", "get", "budget.daily_tokens"]).trim(),
        "7000"
    );
    assert!(!env
        .calls()
        .iter()
        .any(|c| c.contains(&"set".to_string()) && c.contains(&"zephyr".to_string())));
}

/// With the container stopped, a registered key is read by its kind as `/set` reads it, and a
/// preset drops the hand-set classes looser than it, as `/approvals` does.
#[test]
fn config_set_while_stopped_reads_a_registered_key_by_its_kind() {
    let env = Env::new("config-stopped-kinds");
    env.init(&["--no-start", "--preset", "full"]);
    let stopped = [("SHIM_STATE", "exited")];
    let set = |key: &str, value: &str| {
        let out = env.zephyr_env(&["config", "set", key, value], &stopped);
        assert!(out.status.success(), "{key} = {value}: {out:?}");
    };
    set("approvals.messages", "auto");
    set("approvals.posts", "ask");
    set("approvals.preset", "manual");
    set("review.at", "07:30");
    set("approvals.strong_only", "loosen,money");
    set("budget.daily_tokens", "7000");
    set("budget.daily_tokens", "none");
    let config = env.config();
    assert!(config.contains("preset = \"manual\""), "{config}");
    assert!(
        !config.contains("messages ="),
        "looser than manual: {config}"
    );
    assert!(config.contains("posts = \"ask\""), "as strict: {config}");
    assert!(config.contains("at = \"07:30\""), "{config}");
    assert!(
        config.contains("strong_only = [\"loosen\", \"money\"]"),
        "{config}"
    );
    assert!(!config.contains("daily_tokens"), "{config}");
}

/// With the container stopped, `config set model` edits what `/model` edits: the model of the
/// account on that provider, moved first, never a top-level `model` the pool ignores.
#[test]
fn config_set_model_while_stopped_moves_the_accounts_model() {
    let env = Env::new("config-stopped-model");
    env.init(&[
        "--no-start",
        "--account",
        "claude-a=anthropic/claude-sonnet-5",
        "--account",
        "codex-a=openai-codex/gpt-5.5",
    ]);
    let out = env.zephyr_env(
        &["config", "set", "model", "openai-codex/gpt-5.4"],
        &[("SHIM_STATE", "exited")],
    );
    assert!(out.status.success(), "{out:?}");
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    let accounts: Vec<(&str, &str)> = cfg["account"]
        .as_array()
        .unwrap()
        .iter()
        .map(|a| (a["name"].as_str().unwrap(), a["model"].as_str().unwrap()))
        .collect();
    assert_eq!(
        accounts,
        [
            ("codex-a", "openai-codex/gpt-5.4"),
            ("claude-a", "anthropic/claude-sonnet-5")
        ]
    );
    assert!(cfg.get("model").is_none(), "{cfg}");
}

/// A connector's rows (`mcp.NAME.hosts`, `.secrets`, `.enabled`) live in its `[[mcp]]` table:
/// with the container stopped the host edits that table, checked as the daemon would.
#[test]
fn config_set_while_stopped_reaches_a_connectors_table() {
    let env = Env::new("config-stopped-mcp");
    env.init(&["--no-start"]);
    env.ok(&["mcp", "add", "mail", "--", "mail-server"]);
    env.ok(&[
        "mcp",
        "add",
        "docs",
        "--url",
        "https://docs.example.com/mcp",
    ]);
    let stopped = [("SHIM_STATE", "exited")];
    let set = |key: &str, value: &str| env.zephyr_env(&["config", "set", key, value], &stopped);
    for (key, value) in [
        ("mcp.mail.hosts", "API.mail.test, mail.test:8443"),
        ("mcp.mail.secrets", "plain"),
        ("mcp.docs.secrets", "surrogate"),
        ("mcp.docs.enabled", "off"),
    ] {
        let out = set(key, value);
        assert!(out.status.success(), "{key}: {out:?}");
        assert!(
            stdout(&out).contains("applies when zephyr starts"),
            "{out:?}"
        );
    }
    let config = env.config();
    let mail = config
        .split("[[mcp]]")
        .find(|t| t.contains("\"mail\""))
        .unwrap();
    assert!(
        mail.contains("hosts = \"API.mail.test, mail.test:8443\""),
        "{config}"
    );
    assert!(mail.contains("secrets = \"plain\""), "{config}");
    let docs = config
        .split("[[mcp]]")
        .find(|t| t.contains("\"docs\""))
        .unwrap();
    assert!(
        docs.contains("secrets = \"surrogate\"") && docs.contains("enabled = false"),
        "{config}"
    );
    assert_eq!(
        env.ok(&["config", "get", "mcp.mail.secrets"]).trim(),
        "plain"
    );
    // What the daemon would refuse is refused here, and nothing is written.
    for (key, value) in [
        ("mcp.mail.hosts", "*.mail.test"),
        ("mcp.mail.secrets", "raw"),
        ("mcp.nope.hosts", "x.test"),
    ] {
        assert!(!set(key, value).status.success(), "{key} = {value}");
    }
    assert_eq!(env.config(), config);
    // One secret cannot be a plain connector's and another's stand-in: the command that would
    // make it so is refused, here as in the daemon.
    env.ok(&["mcp", "add", "db", "--plain", "--", "db", "${SHARED}"]);
    let pair = [
        "mcp",
        "add",
        "api",
        "--host",
        "api.test",
        "--",
        "api",
        "${SHARED}",
    ];
    let out = env.zephyr(&pair);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success() && said.contains("one name cannot be both"),
        "{said}"
    );
    env.ok(&[
        "mcp", "add", "api", "--host", "api.test", "--", "api", "${OTHER}",
    ]);
    let out = set("mcp.api.secrets", "plain");
    assert!(out.status.success(), "{out:?}");
    env.ok(&[
        "mcp", "add", "api2", "--host", "api.test", "--", "api", "${THIRD}",
    ]);
    // When the config as it is does not load, there is no "before" to compare with: the new
    // one alone must hold no pair.
    let good = env.config();
    let broken = good.replace(
        "name = \"api2\"",
        "name = \"api2\"\nurl = \"https://both.test\" # and a command: not a connector",
    );
    assert!(broken != good);
    std::fs::write(env.daemon().join("zephyr.toml"), &broken).unwrap();
    let mend = [
        "mcp",
        "add",
        "api2",
        "--host",
        "api.test",
        "--",
        "api",
        "${SHARED}",
    ];
    let out = env.zephyr(&mend);
    let said = String::from_utf8_lossy(&out.stderr).to_string();
    assert!(
        !out.status.success() && said.contains("one name cannot be both"),
        "{said}"
    );
    std::fs::write(env.daemon().join("zephyr.toml"), &good).unwrap();
    // A pair that is there already (a hand edit) does not stop an unrelated change.
    let path = env.daemon().join("zephyr.toml");
    let paired = env.config().replace("\"${THIRD}\"", "\"${SHARED}\"");
    assert!(paired.contains("api2") && paired != env.config());
    std::fs::write(&path, paired).unwrap();
    assert!(set("budget.daily_tokens", "7000").status.success());
    assert!(set("mcp.mail.enabled", "off").status.success());
}

#[test]
fn config_get_and_set_check_the_daemon_schema() {
    let env = Env::new("config");
    env.init(&["--no-start"]);
    // A stopped container: the file is edited here (a running daemon's registry settings go
    // through it instead).
    let stopped = [("SHIM_STATE", "exited")];
    let set = |k: &str, v: &str| env.zephyr_env(&["config", "set", k, v], &stopped);
    assert_eq!(env.ok(&["config", "get", "attach.port"]).trim(), "7790");
    assert!(set("budget.daily_tokens", "5000").status.success());
    assert_eq!(
        env.ok(&["config", "get", "budget.daily_tokens"]).trim(),
        "5000"
    );
    assert!(set("timezone", "Asia/Seoul").status.success());
    assert_eq!(env.ok(&["config", "get", "timezone"]).trim(), "Asia/Seoul");
    // A running daemon reloads a key the registry does not know; a stopped one picks the file
    // up when it starts.
    env.calls();
    env.ok(&["config", "set", "owners.discord", "[\"1234\"]"]);
    find(
        &env.calls(),
        &["docker", "exec", CONTAINER, "zephyr", "reload"],
    );
    let out = set("budget.daily_tokens", "7000");
    assert!(
        stdout(&out).contains("applies when zephyr starts"),
        "{out:?}"
    );
    assert!(!env
        .calls()
        .iter()
        .any(|c| c.contains(&"reload".to_string())));
    let before = env.config();
    assert!(!set("timezone", "Mars/Olympus").status.success());
    assert!(!set("typo", "1").status.success());
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
    // Stopped, so the file is edited here rather than through the daemon.
    let set = |v: &str| {
        let key = if v.parse::<u32>().is_ok() {
            "tasks.parallel"
        } else {
            "tasks.model"
        };
        env.zephyr_env(&["config", "set", key, v], &[("SHIM_STATE", "exited")])
    };
    assert!(set("16").status.success());
    assert!(set("claude-code/claude-haiku-5-5").status.success());
    assert!(!set("haiku").status.success());
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
            // The daemon listens on the container's own address under the network floor.
            "172.18.0.2:7790",
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
    find(&calls, &["docker", "network", "rm", CONTAINER]);
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
        // The agent's shell has no route out but the proxy (DESIGN §4.20).
        assert!(
            has_pair(call, "-e", "HTTPS_PROXY=http://127.0.0.1:7792"),
            "{call:?}"
        );
        assert!(
            has_pair(call, "-e", "NO_PROXY=localhost,127.0.0.1,::1"),
            "{call:?}"
        );
    }
    // What counts is the container that runs, not what host.toml asks the next one to be: a
    // host-networking container from before the floor has no proxy to point at ...
    assert!(env
        .on_a_tty("shell", &[("SHIM_NETMODE", "host")])
        .status
        .success());
    let calls = env.calls();
    let call = find_agent_with(&calls, &[], &["-it", "-e", "TERM"]);
    assert!(!call.iter().any(|a| a.contains("PROXY")), "{call:?}");
    // ... and one under the floor keeps it until `zephyr up` makes the host-networking one.
    std::fs::write(env.daemon().join("host.toml"), "network = \"host\"\n").unwrap();
    assert!(env.on_a_tty("shell", &[]).status.success());
    find_agent(&env.calls(), &["-it", "-e", "TERM"]);
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

    env.ok(&["config", "set", "attach.port", "7791"]);
    let calls = env.calls();
    let get = ["zephyr", "__file", "get", "zephyr.toml"];
    let put = ["zephyr", "__file", "put", "zephyr.toml"];
    let root = ["docker", "exec", "-i", "-u", "0", CONTAINER];
    find(&calls, &[&root[..], &get[..]].concat());
    find(&calls, &[&root[..], &put[..]].concat());
    let text = std::fs::read_to_string(files.join("zephyr.toml")).unwrap();
    assert!(text.contains("port = 7791"), "{text}");

    // Stopped: a throwaway root container with no capabilities and no network.
    let out = env.zephyr_env(
        &["config", "get", "attach.port"],
        &[("SHIM_STATE", "exited")],
    );
    assert_eq!(stdout(&out).trim(), "7791");
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

/// The `docker run -d` of the last `up`.
fn run_call(env: &Env) -> Vec<String> {
    find(&env.calls(), &["docker", "run", "-d", "--name", CONTAINER]).clone()
}

#[test]
fn up_creates_the_network_only_when_missing() {
    let env = Env::new("net-create");
    env.init(&["--no-start"]);
    let out = env.zephyr_env(&["up"], &[("SHIM_NO_NETWORK", "1")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = env.calls();
    let create = [
        "docker", "network", "create", "--driver", "bridge", CONTAINER,
    ];
    let at = |prefix: &[&str]| {
        (calls.iter()).position(|c| c.len() >= prefix.len() && c[..prefix.len()] == *prefix)
    };
    let (made, ran) = (at(&create), at(&["docker", "run", "-d"]));
    assert!(made.is_some() && made < ran, "{calls:#?}");
    assert!(!calls[made.unwrap()]
        .iter()
        .any(|a| a == "--ipv6" || a == "--subnet"));
}

#[test]
fn host_networking_in_host_toml_keeps_todays_arguments() {
    let env = Env::new("net-host");
    env.init(&["--no-start"]);
    let host = env.daemon().join("host.toml");
    let text = std::fs::read_to_string(&host).unwrap();
    let with = |mode: &str| text.replace("network = \"bridge\"", &format!("network = \"{mode}\""));
    std::fs::write(&host, with("host")).unwrap();
    env.ok(&["up"]);
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(has_pair(run, "--network", "host"), "{run:?}");
    for absent in [
        "NET_ADMIN",
        "SETPCAP",
        "-p",
        "--hostname",
        "ZEPHYR_NETWORK=bridge",
    ] {
        assert!(!run.iter().any(|a| a == absent), "{absent} in {run:?}");
    }
    assert!(!calls.iter().any(|c| c[1] == "network"), "{calls:#?}");
    // The container that runs decides where the daemon listens, not host.toml.
    for (mode, at) in [("host", "127.0.0.1:7790"), ("", "172.18.0.2:7790")] {
        let out = env.zephyr_env(&["attach", "--stock"], &[("SHIM_NETMODE", mode)]);
        assert!(out.status.success());
        let stock = env.calls();
        let connect = stock.iter().find(|c| c.iter().any(|a| a == "--connect"));
        assert_eq!(connect.unwrap().last().unwrap(), at);
    }

    std::fs::write(&host, with("none")).unwrap();
    let out = env.zephyr(&["up"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("network in host.toml must be \"bridge\" or \"host\""),
        "{err}"
    );
    assert!(
        !env.calls().iter().any(|c| c[1] == "run"),
        "nothing was started"
    );
}

#[test]
fn imessage_is_published_on_loopback_with_a_tailscale_serve_hint() {
    let env = Env::new("net-imessage");
    env.init(&["--no-start"]);
    let mut cfg = env.config();
    cfg.push_str(
        "\n[imessage]\nlisten = \"100.64.0.1:7791\"\nserver_url = \"http://100.64.0.2:1234\"\n\
         password_env = \"ZEPHYR_BB_PASSWORD\"\nsecret_env = \"ZEPHYR_BB_SECRET\"\n",
    );
    // A key from a newer zephyr than this CLI: the ports are read by key, so it is no obstacle.
    cfg.push_str("\n[future]\nsetting = 1\n");
    std::fs::write(env.daemon().join("zephyr.toml"), cfg).unwrap();
    const HINT: &str = "the iMessage webhook is on this host's 127.0.0.1:7791 now; forward the \
        Tailscale address to it once: sudo tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791";
    // Moving over from host networking: the webhook's address changes, and `up` says so.
    let out = env.zephyr_env(&["up"], &[("SHIM_NETMODE", "host")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains(HINT), "{}", stdout(&out));
    let run = run_call(&env);
    assert!(has_pair(&run, "-p", "127.0.0.1:7791:7791"), "{run:?}");
    assert!(has_pair(&run, "-p", "127.0.0.1:7790:7790"), "{run:?}");
    assert!(
        has_pair(&run, "-e", "ZEPHYR_PUBLISHED=7790,7791"),
        "{run:?}"
    );
    // Never the Tailscale address: Docker could not bind it before tailscaled is up.
    assert!(!run.iter().any(|a| a.contains("100.64.0.1")), "{run:?}");
    // Every later `up` has nothing new to say.
    assert!(!env.ok(&["up"]).contains("tailscale serve"));
}

#[test]
fn config_set_of_a_published_port_recreates_the_container() {
    let env = Env::new("net-port");
    env.init(&[]);
    env.calls();
    env.ok(&["config", "set", "attach.port", "7799"]);
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(has_pair(run, "-p", "127.0.0.1:7799:7799"), "{run:?}");
    assert!(!run.iter().any(|a| a.contains("7790")), "{run:?}");
    let reload = ["zephyr", "reload"].map(String::from);
    assert!(!calls.iter().any(|c| c.ends_with(&reload)), "{calls:#?}");
    // Any other key is the daemon's to reload, with no new container.
    env.ok(&["config", "set", "timezone", "UTC"]);
    let calls = env.calls();
    assert!(!calls.iter().any(|c| c[1] == "run"), "{calls:#?}");
    // Stopped: the next `up` publishes it.
    let out = env.zephyr_env(
        &["config", "set", "attach.port", "7798"],
        &[("SHIM_STATE", "exited")],
    );
    assert!(out.status.success());
    assert!(!env.calls().iter().any(|c| c[1] == "run"));
}

#[test]
fn up_from_a_host_networking_container_explains_the_floor() {
    const NOTICE: &str =
        "the agent now reaches the internet only through zephyr's proxy: ports 80 \
        and 443 to public addresses (DESIGN §4.20). Other destinations: /set network.allow \
        HOST:PORT. Sign-ins: paste the address the browser cannot open. To go back: network = \
        \"host\" in host.toml, then zephyr up.";
    let env = Env::new("net-migrate");
    env.init(&["--no-start"]);
    let out = env.zephyr_env(&["up"], &[("SHIM_NETMODE", "host")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(stdout(&out).contains(NOTICE), "{}", stdout(&out));
    // Already on the bridge: nothing to explain.
    assert!(!env.ok(&["up"]).contains("zephyr's proxy"));
}

#[test]
fn doctor_warns_about_a_container_on_host_networking() {
    const WARNING: &str =
        "warn the container still runs host networking; zephyr up moves it to the network floor";
    let env = Env::new("doctor-netmode");
    env.init(&[]);
    let old = stdout(&env.zephyr_env(&["doctor"], &[("SHIM_NETMODE", "host")]));
    assert!(old.contains(WARNING), "{old}");
    let moved = stdout(&env.zephyr(&["doctor"]));
    assert!(!moved.contains("host networking"), "{moved}");
    // Asked for in host.toml, it is no surprise.
    std::fs::write(env.daemon().join("host.toml"), "network = \"host\"\n").unwrap();
    let asked = stdout(&env.zephyr_env(&["doctor"], &[("SHIM_NETMODE", "host")]));
    assert!(!asked.contains(WARNING), "{asked}");
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

/// The prompts `channel add imessage` asks, answered for a webhook on port 7791.
const IMESSAGE_STEPS: [(&str, &str); 4] = [
    ("Webhook listen address", "100.64.0.1:7791\n"),
    ("BlueBubbles server URL", "http://100.64.0.2:1234\n"),
    ("Your iMessage handles", "me@icloud.com\n"),
    ("BlueBubbles server password (hidden)", "pw\n"),
];
const FORWARD_Q: &str = "Forward this machine's Tailscale port 7791 to the webhook?";
const FORWARD: [&str; 6] = [
    "tailscale",
    "serve",
    "--bg",
    "--tcp",
    "7791",
    "tcp://127.0.0.1:7791",
];

/// Under the network floor BlueBubbles reaches the webhook only through `tailscale serve`, so
/// `channel add imessage` offers the forward when it is missing, and only then.
#[test]
fn channel_add_imessage_offers_the_tailscale_forward() {
    let env = Env::new("channel-forward");
    env.init(&["--no-start"]);
    let add = |answer: Option<&str>, vars: &[(&str, &str)]| {
        let mut steps = IMESSAGE_STEPS.to_vec();
        steps.extend(answer.map(|a| (FORWARD_Q, a)));
        let (code, shown) = env.on_a_driven_tty_env(&["channel", "add", "imessage"], &steps, vars);
        assert!(
            shown.contains("LEFT 0"),
            "every prompt was answered: {shown}"
        );
        (code, shown, env.calls())
    };
    let serves = |calls: &[Vec<String>]| at(calls, &["tailscale", "serve", "--bg"], "");

    let (code, shown, calls) = add(Some("y"), &[]);
    assert_eq!(code, 0, "{shown}");
    find(&calls, &FORWARD);
    assert!(env.config().contains("listen = \"100.64.0.1:7791\""));

    // Already forwarded: not asked, not run.
    let status = "tcp://box.tail1234.ts.net:7791 (tailnet only)\n|--> tcp://127.0.0.1:7791";
    let (code, shown, calls) = add(None, &[("SHIM_TS_SERVE", status)]);
    assert_eq!(code, 0, "{shown}");
    assert!(!shown.contains(FORWARD_Q), "{shown}");
    assert_eq!(serves(&calls), None, "{calls:#?}");

    // Refused for want of operator rights: the sudo form, and the channel stays saved.
    let (code, shown, calls) = add(Some("y"), &[("SHIM_TS_DENIED", "1")]);
    assert_eq!(code, 0, "{shown}");
    find(&calls, &FORWARD);
    assert!(
        shown.contains("sudo tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791"),
        "{shown}"
    );
    assert!(env.config().contains("[imessage]"));

    // Declined: the command to run later.
    let (code, shown, calls) = add(Some("n"), &[]);
    assert_eq!(code, 0, "{shown}");
    assert_eq!(serves(&calls), None, "{calls:#?}");
    assert!(
        shown.contains(
            "forward it later: sudo tailscale serve --bg --tcp 7791 tcp://127.0.0.1:7791"
        ),
        "{shown}"
    );
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
fn channel_list_and_remove_cover_telegram_and_signal() {
    let env = Env::new("channel-tg-sig");
    env.init(&["--no-start"]);
    env.ok(&[
        "config",
        "set",
        "telegram.token_env",
        "ZEPHYR_TELEGRAM_TOKEN",
    ]);
    env.ok(&["config", "set", "owners.telegram", "[\"55\"]"]);
    env.ok(&["config", "set", "signal.account", "\"+15550100\""]);
    env.ok(&["config", "set", "owners.signal", "[\"7f3c\"]"]);
    let secrets = env.daemon().join("secrets.env");
    std::fs::write(&secrets, "ZEPHYR_TELEGRAM_TOKEN=t\nOTHER=keep\n").unwrap();
    assert_eq!(
        env.ok(&["channel", "list"]),
        "chat     owners: 0\ntelegram owners: 55\nsignal   owners: 7f3c\n"
    );

    env.calls();
    env.ok(&["channel", "remove", "telegram"]);
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert!(cfg.get("telegram").is_none() && cfg["owners"].get("telegram").is_none());
    assert_eq!(std::fs::read_to_string(&secrets).unwrap(), "OTHER=keep\n");

    // The account's keys go too: signal-cli unregisters, and its folder is deleted.
    env.ok(&["channel", "remove", "signal"]);
    let calls = env.calls();
    find(&calls, &["docker", "exec", CONTAINER, "zephyr", "reload"]);
    find(
        &calls,
        &[
            "docker",
            "exec",
            CONTAINER,
            "zephyr",
            "signal-setup",
            "remove",
        ],
    );
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert!(cfg.get("signal").is_none() && cfg["owners"].get("signal").is_none());

    env.ok(&[
        "channel",
        "signal",
        "trust",
        "7f3c0000-0000-4000-8000-000000000000",
    ]);
    find(
        &env.calls(),
        &[
            "docker",
            "exec",
            CONTAINER,
            "zephyr",
            "signal-setup",
            "trust",
            "7f3c0000-0000-4000-8000-000000000000",
        ],
    );
}

/// Whether a recorded call ends with `argv`.
fn ends(call: &[String], argv: &[&str]) -> bool {
    call.len() >= argv.len()
        && call[call.len() - argv.len()..]
            .iter()
            .zip(argv)
            .all(|(a, b)| a == b)
}

#[test]
fn channel_add_fails_when_doctor_does() {
    let env = Env::new("channel-add-doctor");
    env.init(&["--no-start"]);
    let fail = [("SHIM_DOCTOR_EXIT", "1")];
    let discord = [
        ("Your Discord user id", "1234\n"),
        ("Discord bot token (hidden)", "tok\n"),
    ];
    let (code, shown) = env.on_a_driven_tty_env(&["channel", "add", "discord"], &discord, &fail);
    assert_ne!(code, 0, "{shown}");
    let telegram = [("Telegram bot token (hidden)", "tok\n")];
    let (code, shown) = env.on_a_driven_tty_env(&["channel", "add", "telegram"], &telegram, &fail);
    assert_ne!(code, 0, "{shown}");
    // The pairing code is asked for first: doctor's verdict comes after it.
    let calls = env.calls();
    let at = |argv: &[&str]| calls.iter().rposition(|c| ends(c, argv)).unwrap();
    assert!(at(&["zephyr", "pair", "telegram"]) < at(&["zephyr", "doctor"]));
    let (code, shown) = env.on_a_driven_tty(&["channel", "add", "telegram"], &telegram);
    assert_eq!(code, 0, "{shown}");
}

#[test]
fn adding_signal_again_keeps_the_old_config_until_the_new_number_works() {
    let env = Env::new("channel-add-signal-again");
    env.init(&["--no-start"]);
    env.ok(&["config", "set", "signal.account", "\"+15550100\""]);
    env.calls();
    let steps = [
        ("Signal", "\n"),
        ("Signal number", "+15550199\n"),
        ("voice call", "n"),
    ];
    // The registration fails: nothing changed, and nothing was reloaded first.
    let failing = [("SHIM_SETUP_EXIT", "1")];
    let (code, shown) = env.on_a_driven_tty_env(&["channel", "add", "signal"], &steps, &failing);
    assert_ne!(code, 0, "{shown}");
    let calls = env.calls();
    assert!(
        calls
            .iter()
            .any(|c| ends(c, &["zephyr", "signal-setup", "register", "+15550199"])),
        "{calls:?}"
    );
    assert!(
        !calls.iter().any(|c| ends(c, &["zephyr", "reload"])),
        "{calls:?}"
    );
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(cfg["signal"]["account"].as_str(), Some("+15550100"));
    // It works: the new number is written after it.
    let (code, shown) = env.on_a_driven_tty(&["channel", "add", "signal"], &steps);
    assert_eq!(code, 0, "{shown}");
    let cfg: toml::Value = toml::from_str(&env.config()).unwrap();
    assert_eq!(cfg["signal"]["account"].as_str(), Some("+15550199"));
}

#[test]
fn channel_add_of_telegram_or_signal_needs_a_terminal() {
    let env = Env::new("channel-add-tty");
    env.init(&["--no-start"]);
    for kind in ["telegram", "signal"] {
        let out = env.zephyr(&["channel", "add", kind]);
        assert!(!out.status.success(), "{kind}");
        let err = String::from_utf8_lossy(&out.stderr);
        assert!(err.contains("run it in a terminal"), "{kind}: {err}");
    }
    let out = env.zephyr(&["channel", "add", "whatsapp"]);
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("usage: zephyr channel add|remove discord|imessage|telegram|signal"),
        "{err}"
    );
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

/// With nothing set, the CLI runs this owner's published release, as it always has.
#[test]
fn default_image_is_the_owners_release() {
    let env = Env::new("default-image");
    let projects = env.projects.display().to_string();
    let args = ["init", "--non-interactive", "--projects", &projects];
    let out = env.zephyr_env(&args, &[("ZEPHYR_IMAGE", "")]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let image = format!("ghcr.io/yansigit/zephyr:{}", env!("CARGO_PKG_VERSION"));
    let calls = env.calls();
    find(&calls, &["docker", "image", "inspect", &image]);
    assert_eq!(find(&calls, RUN).last(), Some(&image));
}

#[test]
fn zephyr_repo_moves_the_image_and_keeps_the_gh_token_at_ghcr() {
    let env = Env::new("zephyr-repo");
    let cli = env.cli_copy();
    let me = env!("CARGO_PKG_VERSION");
    let repo = "registry.example.com/me/zephyr";
    let vars = [
        ("ZEPHYR_IMAGE", ""),
        ("ZEPHYR_REPO", repo),
        ("SHIM_LATEST", "99.0.0"),
        ("SHIM_VERSION", me),
    ];
    let out = env.run_cli(&cli, &["upgrade"], &vars);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let calls = env.calls();
    find(&calls, &["docker", "pull", &format!("{repo}:latest")]);
    let cat = ["docker", "run", "--rm", "--entrypoint", "cat"];
    assert_eq!(find(&calls, &cat)[5], format!("{repo}:99.0.0"));

    // Another registry is never handed the gh token: the owner logs in to it.
    let _ = std::fs::remove_file(env.dir.join("env.log"));
    let failing = [vars[0], vars[1], ("SHIM_PULL_FAIL", "1")];
    let out = env.zephyr_env(&["upgrade"], &failing);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains(&format!("could not pull {repo}:latest")),
        "{err}"
    );
    assert!(err.contains("docker login registry.example.com"), "{err}");
    let calls = env.calls();
    assert_eq!(at(&calls, &["docker", "login"], ""), None, "{calls:#?}");
    assert_eq!(at(&calls, &["gh", "api", "user"], ""), None, "{calls:#?}");
    let env_log = std::fs::read_to_string(env.dir.join("env.log")).unwrap_or_default();
    assert!(!env_log.contains("GH_TOKEN"), "{env_log}");

    // Empty is unset: the default repository.
    let out = env.zephyr_env(
        &["upgrade"],
        &[vars[0], ("ZEPHYR_REPO", ""), ("SHIM_PULL_FAIL", "1")],
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("could not pull ghcr.io/yansigit/zephyr:latest"),
        "{err}"
    );
}

/// image.yml publishes to this repository's own package, and tells the image's CLI so.
#[test]
fn the_image_workflow_tags_the_repository_it_runs_in() {
    let yml = read(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/image.yml"
    )));
    assert!(yml.contains(r#"echo "image=ghcr.io/${GITHUB_REPOSITORY,,}""#));
    assert!(yml.contains("ZEPHYR_BUILD_REPO=${{ steps.v.outputs.image }}"));
    assert!(yml.contains("${{ steps.v.outputs.image }}:${{ steps.v.outputs.version }}"));
    let tags = yml.lines().filter(|l| l.contains("ghcr.io/yansigit"));
    assert!(
        tags.clone().all(|l| l.trim_start().starts_with('#')),
        "{:?}",
        tags.collect::<Vec<_>>()
    );
}

/// The tag build smoke-tests every extension the CLI lists; a manual `providers: none` run builds
/// the image without the owner's provider sources and checks that they are absent.
#[test]
fn the_image_workflow_smoke_tests_every_extension() {
    let yml = read(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../.github/workflows/image.yml"
    )));
    assert!(yml.contains("test -f /usr/local/lib/libzephyr_policy.so"));
    for ext in zephyr::host::conf::EXTENSIONS {
        assert!(yml.contains(&format!("test -f {ext}")), "{ext}");
    }
    for want in [
        "providers:",
        "type: choice",
        "default: full",
        "options: [full, none]",
        "RPI_PACKAGE_REPO=",
        "PLUGINS_REPO=",
        "test ! -e /opt/zephyr/ext/librpi_providers.so",
        "test ! -e /opt/zephyr/ext/librpi_mcp_adapter.so",
        "test ! -e /opt/zephyr/ext/rpi-claude-agent",
    ] {
        assert!(yml.contains(want), "{want}");
    }
}

/// A crate's build script must never read a GitHub token: GH_TOKEN is set only in a step's own
/// `env` (never a job's or the workflow's), a step that sets it runs no cargo (nor `ci.sh run`, nor
/// bump-rpi.sh's `cargo update`), `ci.sh rescue` runs its checks without it, and no checkout keeps
/// its token in `.git/config`.
#[test]
fn no_step_that_runs_cargo_holds_a_github_token() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let indent = |l: &str| l.len() - l.trim_start().len();
    let code = |l: &&str| !l.trim().is_empty() && !l.trim_start().starts_with('#');
    let mut tokens = 0;
    for entry in std::fs::read_dir(root.join(".github/workflows")).unwrap() {
        let path = entry.unwrap().path();
        let text = read(&path);
        let lines: Vec<&str> = text.lines().filter(code).collect();
        // The lines of the block that starts at `at`: it and everything indented deeper.
        let block = |at: usize| {
            let end = (at + 1..lines.len()).find(|&j| indent(lines[j]) <= indent(lines[at]));
            lines[at..end.unwrap_or(lines.len())].join("\n")
        };
        let parent = |at: usize| {
            (0..at)
                .rev()
                .find(|&j| indent(lines[j]) < indent(lines[at]))
        };
        for (i, line) in lines.iter().enumerate() {
            if line.trim_start().starts_with("- uses: actions/checkout@") {
                assert!(
                    block(i).contains("persist-credentials: false"),
                    "{path:?}: {}",
                    block(i)
                );
            }
            if !line.trim_start().starts_with("GH_TOKEN:") {
                continue;
            }
            tokens += 1;
            let env = parent(i).unwrap();
            let step = match lines[env].trim() {
                "- env:" => Some(env),
                "env:" => parent(env).filter(|&s| lines[s].trim_start().starts_with("- ")),
                other => panic!("{path:?}: GH_TOKEN under {other}"),
            }
            .unwrap_or_else(|| panic!("{path:?}: GH_TOKEN outside a step: {line}"));
            let step = block(step);
            let words: Vec<&str> = step
                .split(|c: char| c.is_whitespace() || c == ';')
                .collect();
            let ci_run = words.windows(2).any(|w| w == [".github/ci.sh", "run"]);
            let cargo = ["cargo", "deploy/bump-rpi.sh"]
                .iter()
                .any(|c| words.contains(c));
            assert!(!ci_run && !cargo, "{path:?}:\n{step}");
        }
    }
    assert!(tokens >= 6, "{tokens}");
    let ci = read(&root.join(".github/ci.sh"));
    assert!(
        ci.contains("( unset GH_TOKEN; check \"${name% (*}\" )"),
        "{ci}"
    );
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

    // A provider's key for the daemon's own use (DESIGN §4.26): from the prompt, never argv.
    const API_KEY: &str = "KEY0123456789telnyx";
    let service = [
        "vault",
        "add",
        "service",
        "calls",
        "--provider",
        "telnyx",
        "--account",
        "1494404757140276705",
        "--from",
        "+14165550100",
        "--public-key",
        "AAECAwQFBgcICQoLDA0ODxAREhMUFRYXGBkaGxwdHh8=",
    ];
    let out = env.zephyr_input(&service, &format!("{API_KEY}\n"));
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let saved = std::fs::read_to_string(files.join("vault.toml")).unwrap();
    assert!(saved.contains("[[service]]") && saved.contains(API_KEY));
    let list = env.ok(&["vault", "list"]);
    assert!(
        list.contains("service calls: telnyx; a key is saved"),
        "{list}"
    );
    let output = std::fs::read_to_string(env.dir.join("output.log")).unwrap();
    assert!(!format!("{:?}", env.calls()).contains(API_KEY) && !output.contains(API_KEY));
    assert!(!env
        .zephyr_input(&["vault", "add", "service", "cards"], "k\n")
        .status
        .success());
    // Rotating the key alone: no other flag, and the saved ids stay.
    let rotated = "KEYrotated9876543210";
    let out = env.zephyr_input(
        &["vault", "add", "service", "calls"],
        &format!("{rotated}\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let saved = std::fs::read_to_string(files.join("vault.toml")).unwrap();
    assert!(
        saved.contains(rotated) && !saved.contains(API_KEY),
        "{saved}"
    );
    assert!(saved.contains("+14165550100") && saved.contains("1494404757140276705"));
    // `rm` says what it removed, and never leaves a provider key behind while saying so. A
    // login of the same name, from a hand-edited file, goes first; the service stays until it
    // is named.
    let both = format!(
        "[[login]]\nname = \"calls\"\nsite = \"https://example.com\"\nusername = \"u\"\n\
         password = \"p\"\n\n{saved}"
    );
    std::fs::write(files.join("vault.toml"), both).unwrap();
    assert_eq!(
        env.ok(&["vault", "rm", "calls"]).trim(),
        "removed the login calls; the service calls is still saved: zephyr vault rm service \
         calls removes it"
    );
    assert!(std::fs::read_to_string(files.join("vault.toml"))
        .unwrap()
        .contains(rotated));
    assert_eq!(
        env.ok(&["vault", "rm", "service", "calls"]).trim(),
        "removed the service calls and its provider key"
    );
    assert!(!std::fs::read_to_string(files.join("vault.toml"))
        .unwrap()
        .contains(rotated));
    assert!(!env
        .zephyr(&["vault", "rm", "service", "calls"])
        .status
        .success());
    assert!(!env.zephyr(&["vault", "rm", "calls"]).status.success());

    // A token's hosts are taken where the owner saves it (DESIGN §4.21), normalized; a wildcard
    // saves nothing; a new value keeps them.
    let add = [
        "vault",
        "add",
        "token",
        "API_KEY",
        "--host",
        "API.example.com",
    ];
    let out = env.zephyr_input(
        &[
            &add[..],
            &["--host", "example.com:8443", "--header", "X-Api-Key"],
        ]
        .concat(),
        "k-123456\n",
    );
    let said = stdout(&out);
    assert!(
        said.contains("it is sent only to api.example.com, example.com:8443"),
        "{said}"
    );
    let saved = std::fs::read_to_string(files.join("vault.toml")).unwrap();
    assert!(
        saved.contains("hosts = [\"api.example.com:443\", \"example.com:8443\"]"),
        "{saved}"
    );
    // And the headers it is swapped in besides Authorization.
    assert!(saved.contains("headers = [\"x-api-key\"]"), "{saved}");
    let wild = ["vault", "add", "token", "WILD", "--host", "*.example.com"];
    assert!(!env.zephyr_input(&wild, "k-123456\n").status.success());
    assert!(!std::fs::read_to_string(files.join("vault.toml"))
        .unwrap()
        .contains("WILD"));
    assert!(env
        .zephyr_input(&["vault", "edit", "API_KEY"], "k-rotated\n")
        .status
        .success());
    let saved = std::fs::read_to_string(files.join("vault.toml")).unwrap();
    assert!(
        saved.contains("k-rotated") && saved.contains("example.com:8443"),
        "{saved}"
    );
    let list = env.ok(&["vault", "list"]);
    assert!(list.contains("token API_KEY: not allowed for any program yet; sent only to api.example.com, example.com:8443"), "{list}");
    assert!(
        list.contains("token GH_TOKEN: not allowed for any program yet; bound to no host yet"),
        "{list}"
    );

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
fn facts_add_reads_the_value_hidden_and_lists_names_only() {
    const PHONE: &str = "+1 555 010 2345";
    let env = Env::new("facts");
    env.init(&["--no-start"]);
    // As after `zephyr up`: the daemon's files are root's, reached through `zephyr __file`.
    let files = env.dir.join("files");
    std::fs::create_dir_all(&files).unwrap();
    for name in ["zephyr.toml", "secrets.env", "tui.token", "host.toml"] {
        std::fs::rename(env.daemon().join(name), files.join(name)).unwrap();
    }
    std::fs::set_permissions(env.daemon(), std::fs::Permissions::from_mode(0o000)).unwrap();

    let out = env.zephyr_input(
        &["facts", "add", "phone", "--kind", "digits"],
        &format!("{PHONE}\n"),
    );
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    env.ok(&["facts", "allow", "phone", "mum@example.com"]);
    let list = env.ok(&["facts", "list"]);
    assert!(
        list.contains("phone (digits) allow: mum@example.com"),
        "{list}"
    );

    let calls = env.calls();
    let root = ["docker", "exec", "-i", "-u", "0", CONTAINER];
    for op in ["get", "put"] {
        find(
            &calls,
            &[&root[..], &["zephyr", "__file", op, "facts.toml"]].concat(),
        );
    }
    let saved = std::fs::read_to_string(files.join("facts.toml")).unwrap();
    assert!(saved.contains(PHONE), "{saved}");
    let output = std::fs::read_to_string(env.dir.join("output.log")).unwrap();
    assert!(!format!("{calls:?}").contains(PHONE), "value in argv");
    assert!(!output.contains(PHONE), "value printed");

    // A value never goes in an argument; too short a one is refused, naming no value.
    assert!(!env
        .zephyr(&["facts", "add", "phone", "--kind", "digits", PHONE])
        .status
        .success());
    let out = env.zephyr_input(&["facts", "add", "pin", "--kind", "digits"], "12345\n");
    assert!(!out.status.success());
    assert!(!String::from_utf8_lossy(&out.stderr).contains("12345"));

    env.ok(&["facts", "rm", "phone"]);
    let saved = std::fs::read_to_string(files.join("facts.toml")).unwrap();
    assert!(!saved.contains("phone"), "{saved}");
    assert!(!env.zephyr(&["facts", "rm", "phone"]).status.success());
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

/// A provider source is left out only when its repo arg is empty: with the arg set, a tree
/// without the expected file fails the build instead of shipping an image without the providers.
#[test]
fn the_dockerfile_skips_a_provider_source_only_for_an_empty_arg() {
    let text = read(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/Dockerfile"
    )));
    let stage = |name: &str| {
        let start = text.find(&format!(" AS {name}\n")).unwrap();
        let rest = &text[start..];
        rest[..rest[1..].find("\nFROM ").map_or(rest.len(), |i| i + 1)].to_string()
    };
    for (name, arg, file) in [
        ("rpi-ext", "RPI_PACKAGE_REPO", "/src/rpi-package/Cargo.toml"),
        (
            "plugins",
            "PLUGINS_REPO",
            "packages/rpi-claude-agent/scripts/stage.sh",
        ),
    ] {
        let stage = stage(name);
        assert!(stage.contains(&format!("\nARG {arg}\n")), "{name}: {stage}");
        assert!(
            stage.contains(&format!("if [ -n \"${arg}\" ]; then")),
            "{name}: {stage}"
        );
        assert!(
            !stage.contains(&format!("[ -f {file} ]")),
            "{name}: {stage}"
        );
        assert!(
            !stage.contains(&format!("[ -e {file} ]")),
            "{name}: {stage}"
        );
    }
}

/// release.sh takes the rpi binary from `RPI_BIN` and the runner its repo from `ZEPHYR_GH_REPO`;
/// unset, each is today's value, and bump-rpi.sh's rewrite of the pinned path still matches.
#[test]
fn release_and_runner_scripts_keep_their_defaults() {
    let deploy = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../deploy"));
    for script in ["release.sh", "runner/start.sh", "bump-rpi.sh"] {
        let ok = Command::new("bash")
            .arg("-n")
            .arg(deploy.join(script))
            .status();
        assert!(ok.unwrap().success(), "{script}");
    }
    let release = read(&deploy.join("release.sh"));
    assert!(
        release.contains("rpi=${RPI_BIN:-/home/user/Projects/.always-on/rpi-0.21.2/bin/rpi}"),
        "{release}"
    );
    assert!(release.contains("export RPI_BIN=\"$rpi\""), "{release}");
    // The owner's release installs and upgrades the defaults, whatever this shell points at.
    let unset = "\nunset ZEPHYR_REPO ZEPHYR_BUILD_REPO ZEPHYR_IMAGE ZEPHYR_CONTAINER\n";
    let at = release
        .find(unset)
        .expect("release.sh unsets the CLI's knobs");
    assert!(
        at < release.find("\ncargo install -q").unwrap(),
        "{release}"
    );
    let runner = read(&deploy.join("runner/start.sh"));
    assert!(
        runner.contains("repo=${ZEPHYR_GH_REPO:-yansigit/zephyr}"),
        "{runner}"
    );
    assert!(read(&deploy.join("bump-rpi.sh")).contains("rpi-$old/bin/rpi"));
}

/// release.sh refuses a version that is not above Cargo.toml's before it tests anything, and pushes
/// the release commit and its tag together or not at all.
#[test]
fn release_only_moves_forward_and_pushes_atomically() {
    let release = read(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/release.sh"
    )));
    let newer = release
        .lines()
        .find(|l| l.starts_with("newer() {"))
        .expect("release.sh defines newer()");
    for (version, current, ok) in [
        ("0.2.1", "0.2.0", true),
        ("0.10.0", "0.9.9", true),
        ("1.0.0", "0.99.0", true),
        ("0.2.0", "0.2.0", false),
        ("0.1.9", "0.2.0", false),
        ("0.9.0", "0.10.0", false),
    ] {
        let status = Command::new("bash")
            .arg("-c")
            .arg(format!("{newer}\nnewer {version} {current}"))
            .status()
            .unwrap();
        assert_eq!(status.success(), ok, "{version} over {current}");
    }
    let check = "newer \"$version\" \"$current\" || die";
    assert!(
        release.find(check).unwrap() < release.find("\nstep \"testing\"\n").unwrap(),
        "{release}"
    );
    assert!(
        release.contains("\n  git push --atomic -q origin main \"$tag\"\n"),
        "{release}"
    );
}

/// Every file the image copies from the build context is let back in by `.dockerignore`
/// (`*`, then `!PATH` per allowed path): one left out fails `docker build` at that COPY.
#[test]
fn every_copy_in_the_dockerfile_is_in_the_build_context() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let ignore = std::fs::read_to_string(root.join(".dockerignore")).unwrap();
    assert!(ignore.lines().any(|l| l.trim() == "*"), "{ignore}");
    let allowed: Vec<&str> = (ignore.lines())
        .filter_map(|l| l.trim().strip_prefix('!'))
        .map(|p| p.trim_end_matches('/'))
        .collect();
    let dockerfile = std::fs::read_to_string(root.join("deploy/Dockerfile")).unwrap();
    let mut checked = 0;
    for line in dockerfile.lines().map(str::trim) {
        let Some(args) = line.strip_prefix("COPY ") else {
            continue;
        };
        let words: Vec<&str> = args.split_whitespace().collect();
        if words.iter().any(|w| w.starts_with("--from")) {
            continue;
        }
        for source in &words[..words.len() - 1] {
            let source = source.trim_end_matches('/');
            let ok = (allowed.iter()).any(|a| source == *a || source.starts_with(&format!("{a}/")));
            assert!(ok, "COPY {source} is left out by .dockerignore");
            assert!(root.join(source).exists(), "COPY {source}: no such path");
            checked += 1;
        }
    }
    assert!(checked >= 4, "{checked}");
}

/// Both wrappers on the agent's PATH (`agent-browser`, and `xdotool` for a desktop takeover) are
/// links to this binary in root's gate bin; the real ones stay out of that folder.
#[test]
fn the_gate_bin_holds_both_wrappers() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let dockerfile = std::fs::read_to_string(root.join("deploy/Dockerfile")).unwrap();
    for name in ["agent-browser", "xdotool"] {
        let link = format!("ln -s /usr/local/bin/zephyr /opt/zephyr/gate/bin/{name}");
        assert!(
            dockerfile.contains(&link),
            "no `{link}` in deploy/Dockerfile"
        );
    }
}

const TS_STATUS: &str = r#"{"BackendState":"Running","Self":{"DNSName":"box.tail1234.ts.net."},"CertDomains":["box.tail1234.ts.net"]}"#;

#[test]
fn web_setup_writes_the_host_serves_the_port_and_enrols() {
    let env = Env::new("web-setup");
    env.init(&["--no-start"]);
    env.calls();
    let out = env.zephyr_env(&["web", "setup", "--yes"], &[("SHIM_TS_STATUS", TS_STATUS)]);
    assert!(out.status.success(), "{out:?}");
    let doc: toml::Table = toml::from_str(&env.config()).unwrap();
    assert_eq!(doc["web"]["host"].as_str(), Some("box.tail1234.ts.net"));
    assert_eq!(doc["web"]["port"].as_integer(), Some(7793));
    let calls = env.calls();
    find(&calls, &["tailscale", "status", "--json"]);
    find(
        &calls,
        &["tailscale", "serve", "--bg", "http://127.0.0.1:7793"],
    );
    find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    find(
        &calls,
        &["docker", "exec", CONTAINER, "zephyr", "web", "enrol"],
    );
    let order = |prefix: &[&str]| {
        calls
            .iter()
            .position(|c| c.starts_with(&prefix.iter().map(|s| s.to_string()).collect::<Vec<_>>()))
            .unwrap()
    };
    assert!(
        order(&["tailscale", "serve"]) < order(&["docker", "run", "-d"]),
        "serve, then up"
    );
    assert!(
        order(&["docker", "run", "-d"])
            < order(&["docker", "exec", CONTAINER, "zephyr", "web", "enrol"])
    );
    let text = stdout(&out);
    assert!(
        text.contains("#enrol=ABCDE-FGHJK"),
        "the code is shown: {text}"
    );
    assert!(text.contains("never point tailscale funnel at this port: the web app refuses funnel traffic, and a funnel would put it on the internet"), "{text}");
    // The default strong channels already hold passkey, and no passkey is enrolled yet.
    assert!(
        !calls.iter().any(|c| c.contains(&"set".to_string())),
        "{calls:#?}"
    );
    // Asked again, it changes nothing it has no need to, and asks before each change: with
    // neither a terminal nor --yes it stops before the first.
    let before = env.config();
    let out = env.zephyr_env(&["web", "setup"], &[("SHIM_TS_STATUS", TS_STATUS)]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("--yes"),
        "{out:?}"
    );
    assert_eq!(env.config(), before);
    // `zephyr web enrol NAME` asks the daemon in the container.
    env.calls();
    let out = env.zephyr(&["web", "enrol", "ipad"]);
    assert!(
        out.status.success() && stdout(&out).contains("#enrol="),
        "{out:?}"
    );
    find(
        &env.calls(),
        &[
            "docker", "exec", CONTAINER, "zephyr", "web", "enrol", "ipad",
        ],
    );
    assert_eq!(env.zephyr(&["web", "nope"]).status.code(), Some(1));
}

#[test]
fn web_setup_adds_passkey_to_an_explicit_list_and_offers_money_once_enrolled() {
    let env = Env::new("web-setup-strong");
    env.init(&["--no-start"]);
    let path = env.daemon().join("zephyr.toml");
    let text = format!(
        "{}\n[settings]\nstrong_channels = [\"tui\", \"chat\"]\n",
        env.config()
    );
    std::fs::write(&path, text).unwrap();
    env.calls();
    let out = env.zephyr_env(
        &["web", "setup", "--yes"],
        &[
            ("SHIM_TS_STATUS", TS_STATUS),
            ("SHIM_PASSKEYS", "phone (used 1m ago)"),
        ],
    );
    assert!(out.status.success(), "{out:?}");
    let calls = env.calls();
    let set = ["docker", "exec", CONTAINER, "zephyr", "set"];
    find(
        &calls,
        &[&set[..], &["settings.strong_channels", "tui,chat,passkey"]].concat(),
    );
    find(
        &calls,
        &[&set[..], &["approvals.strong_only", "loosen,money"]].concat(),
    );
}

#[test]
fn web_setup_stops_without_https_certificates() {
    let env = Env::new("web-setup-certs");
    env.init(&["--no-start"]);
    let before = env.config();
    env.calls();
    let no_certs = TS_STATUS.replace(r#"["box.tail1234.ts.net"]"#, "[]");
    let stopped = |status: &[(&str, &str)], said: &str| {
        let out = env.zephyr_env(&["web", "setup", "--yes"], status);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
        assert!(text.contains(said), "{text}");
        assert_eq!(env.config(), before, "zephyr.toml is unchanged");
        let calls = env.calls();
        assert!(
            !calls
                .iter()
                .any(|c| c.len() > 1 && c[0] == "tailscale" && c[1] == "serve"),
            "{calls:#?}"
        );
    };
    stopped(
        &[("SHIM_TS_STATUS", &no_certs)],
        "turn on HTTPS certificates in the Tailscale admin console (DNS → HTTPS Certificates)",
    );
    let logged_out = TS_STATUS.replace("Running", "NeedsLogin");
    stopped(&[("SHIM_TS_STATUS", &logged_out)], "tailscale up");
    stopped(&[], "tailscale up");
    // Without operator rights `serve` is refused: the sudo forms are printed, and nothing is started.
    let out = env.zephyr_env(
        &["web", "setup", "--yes"],
        &[("SHIM_TS_STATUS", TS_STATUS), ("SHIM_TS_DENIED", "1")],
    );
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let text = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(
        text.contains("sudo tailscale serve --bg http://127.0.0.1:7793"),
        "{text}"
    );
    assert!(
        text.contains("sudo tailscale set --operator=$USER"),
        "{text}"
    );
    assert!(!env
        .calls()
        .iter()
        .any(|c| c.len() > 2 && c[1] == "run" && c[2] == "-d"));
}

#[test]
fn status_says_whether_tailscale_serve_forwards_the_port() {
    let env = Env::new("web-status");
    env.init(&["--no-start"]);
    let quiet = stdout(&env.zephyr(&["status"]));
    assert!(!quiet.contains("web:"), "no [web], no line: {quiet}");
    let out = env.zephyr_env(&["web", "setup", "--yes"], &[("SHIM_TS_STATUS", TS_STATUS)]);
    assert!(out.status.success(), "{out:?}");
    let serve = "https://box.tail1234.ts.net (tailnet only)\n|-- / proxy http://127.0.0.1:7793\n";
    let on = stdout(&env.zephyr_env(&["status"], &[("SHIM_TS_SERVE", serve)]));
    assert!(
        on.contains("web: tailscale serve forwards https://box.tail1234.ts.net\n"),
        "{on}"
    );
    for other in ["", "|-- / proxy http://127.0.0.1:8080\n"] {
        let off = stdout(&env.zephyr_env(&["status"], &[("SHIM_TS_SERVE", other)]));
        let line =
            "web: tailscale serve does not forward to 127.0.0.1:7793; run zephyr web setup\n";
        assert!(off.contains(line), "{off}");
    }
}

#[test]
fn up_in_bridge_mode_publishes_the_web_port_on_loopback() {
    let env = Env::new("web-bridge");
    env.init(&["--no-start"]);
    env.calls();
    env.ok(&["up"]);
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(
        !run.iter().any(|a| a.contains("7793")),
        "no [web], no port: {run:?}"
    );
    // A port the owner chose is kept: setup serves and publishes that one.
    let path = env.daemon().join("zephyr.toml");
    let text = format!(
        "{}\n[web]\nhost = \"old.ts.net\"\nport = 8000\n",
        env.config()
    );
    std::fs::write(&path, text).unwrap();
    let out = env.zephyr_env(&["web", "setup", "--yes"], &[("SHIM_TS_STATUS", TS_STATUS)]);
    assert!(out.status.success(), "{out:?}");
    let calls = env.calls();
    find(
        &calls,
        &["tailscale", "serve", "--bg", "http://127.0.0.1:8000"],
    );
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(
        run.windows(2).any(|w| w == ["-p", "127.0.0.1:8000:8000"]),
        "{run:?}"
    );
    assert!(
        run.iter()
            .any(|a| a.starts_with("ZEPHYR_PUBLISHED=") && a.contains("8000")),
        "{run:?}"
    );
    let doc: toml::Table = toml::from_str(&env.config()).unwrap();
    assert_eq!(doc["web"]["host"].as_str(), Some("box.tail1234.ts.net"));
    assert_eq!(doc["web"]["port"].as_integer(), Some(8000));
    // Under host networking nothing is published: the daemon is on the host's loopback itself.
    let host = env.daemon().join("host.toml");
    let text = std::fs::read_to_string(&host).unwrap_or_default();
    let text = match text.contains("network = \"bridge\"") {
        true => text.replace("network = \"bridge\"", "network = \"host\""),
        false => format!("{text}\nnetwork = \"host\"\n"),
    };
    std::fs::write(&host, text).unwrap();
    env.ok(&["up"]);
    let calls = env.calls();
    let run = find(&calls, &["docker", "run", "-d", "--name", CONTAINER]);
    assert!(!run.iter().any(|a| a == "-p"), "{run:?}");
}

#[test]
fn web_setup_needs_the_network_floor() {
    let env = Env::new("web-host-net");
    env.init(&["--no-start"]);
    let host = env.daemon().join("host.toml");
    let text = std::fs::read_to_string(&host).unwrap_or_default();
    let text = match text.contains("network = \"bridge\"") {
        true => text.replace("network = \"bridge\"", "network = \"host\""),
        false => format!("{text}\nnetwork = \"host\"\n"),
    };
    std::fs::write(&host, text).unwrap();
    let before = env.config();
    env.calls();
    let out = env.zephyr_env(&["web", "setup", "--yes"], &[("SHIM_TS_STATUS", TS_STATUS)]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains("the web app needs the network floor; host.toml network = \"bridge\""),
        "{said}"
    );
    assert_eq!(env.config(), before);
    assert!(
        !env.calls().iter().any(|c| c[0] == "tailscale"),
        "nothing was asked of tailscale"
    );
}

/// What every install.sh run leaves out: no sudo, and nothing fetched off the web.
fn fetches_nothing(calls: &[Vec<String>]) {
    let bad = calls
        .iter()
        .find(|c| ["sudo", "curl", "wget"].contains(&c[0].as_str()));
    assert!(bad.is_none(), "{bad:?}");
}

/// The files in `~/.local/bin`, sorted.
fn local_bin(env: &Env) -> Vec<String> {
    let dir = std::fs::read_dir(env.home.join(".local/bin"));
    let mut names: Vec<String> = dir
        .into_iter()
        .flatten()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// A new CLI and `init` would upgrade a live container through `up_checked`, so install.sh never
/// goes on where zephyr already is, even when only its files are left after `zephyr down`.
#[test]
fn install_refuses_an_existing_install() {
    let none = [("SHIM_NO_CONTAINER", "1")];
    // The container; then each thing an install leaves, alone.
    for (name, left) in [
        ("container", ""),
        ("daemon", ".zephyr-daemon"),
        ("home", ".zephyr"),
        ("cli", ".local/bin/zephyr"),
    ] {
        let env = Env::new(&format!("install-refuses-{name}"));
        let vars: &[(&str, &str)] = if left.is_empty() { &[] } else { &none };
        match left {
            "" => {}
            ".local/bin/zephyr" => {
                std::fs::create_dir_all(env.home.join(".local/bin")).unwrap();
                std::fs::write(env.home.join(left), "mine").unwrap();
            }
            _ => std::fs::create_dir_all(env.home.join(left)).unwrap(),
        }
        let out = env.install(&[], vars);
        assert_eq!(out.status.code(), Some(1), "{out:?}");
        let said = String::from_utf8_lossy(&out.stderr);
        assert!(
            said.contains("zephyr is installed here; upgrade with zephyr upgrade"),
            "{said}"
        );
        let calls = env.calls();
        let wrote = calls.iter().find(|c| {
            c.starts_with(&s(&["docker", "pull"])) || c.starts_with(&s(&["cargo", "install"]))
        });
        assert!(wrote.is_none(), "{wrote:?}");
        fetches_nothing(&calls);
        match left {
            ".local/bin/zephyr" => assert_eq!(read(&env.home.join(left)), "mine"),
            _ => assert!(local_bin(&env).is_empty(), "{:?}", local_bin(&env)),
        }
    }
}

#[test]
fn install_from_the_image_writes_the_cli_and_runs_init() {
    let image = "ghcr.io/yansigit/zephyr:latest";
    // The image's `zephyr --version`, which the copied CLI (the shim's: `zephyr <tag>`) must say.
    let none = [("SHIM_NO_CONTAINER", "1"), ("SHIM_LATEST", "latest")];
    let env = Env::new("install-image");
    let out = env.install(&["--from-image", "--", "--non-interactive"], &none);
    assert!(out.status.success(), "{out:?}");
    let calls = env.calls();
    find(&calls, &["docker", "pull", image]);
    let cat = ["docker", "run", "--rm", "--entrypoint", "cat", image];
    let want = s(&[&cat[..], &["/usr/local/bin/zephyr"]].concat());
    assert_eq!(find(&calls, &cat), &want);
    fetches_nothing(&calls);
    assert_eq!(mode(&env.home.join(".local/bin/zephyr")), 0o755);
    assert_eq!(local_bin(&env), ["zephyr"], "no temp file left");
    assert_eq!(read(&env.dir.join("exec.log")), "init --non-interactive\n");

    let env = Env::new("install-image-no-init");
    let out = env.install(&["--from-image", "--no-init"], &none);
    assert!(out.status.success(), "{out:?}");
    assert_eq!(mode(&env.home.join(".local/bin/zephyr")), 0o755);
    assert_eq!(local_bin(&env), ["zephyr"], "no temp file left");
    assert!(!env.dir.join("exec.log").exists(), "init did not run");
    fetches_nothing(&env.calls());
}

/// A private ghcr.io package is pulled after a login as the gh user, the token on stdin; any
/// other registry gets no login and no token.
#[test]
fn install_logs_in_only_at_ghcr_when_the_pull_fails() {
    let env = Env::new("install-pull-fail");
    let failing = [("SHIM_NO_CONTAINER", "1"), ("SHIM_PULL_FAIL", "1")];
    let out = env.install(&["--from-image", "--no-init"], &failing);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let calls = env.calls();
    let login = find(&calls, &["docker", "login"]);
    let want = s(&[
        "docker",
        "login",
        "ghcr.io",
        "-u",
        "tester",
        "--password-stdin",
    ]);
    assert_eq!(login, &want);
    let pulls = calls
        .iter()
        .filter(|c| c.starts_with(&s(&["docker", "pull"])));
    assert_eq!(pulls.count(), 2, "pulled again after the login: {calls:#?}");
    assert!(!format!("{calls:?}").contains(GH_TOKEN));
    fetches_nothing(&calls);
    assert!(local_bin(&env).is_empty(), "{:?}", local_bin(&env));

    let mut vars = failing.to_vec();
    vars.push(("ZEPHYR_REPO", "registry.example.com/me/zephyr"));
    let out = env.install(&["--from-image", "--no-init"], &vars);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let calls = env.calls();
    find(
        &calls,
        &["docker", "pull", "registry.example.com/me/zephyr:latest"],
    );
    let login = calls
        .iter()
        .find(|c| c.starts_with(&s(&["docker", "login"])) || c[0] == "gh");
    assert!(login.is_none(), "{login:?}");
    fetches_nothing(&calls);
    assert!(local_bin(&env).is_empty(), "{:?}", local_bin(&env));
}

/// The CLI copied out must be the image's own: the same `zephyr --version` (as `fetch_cli` checks).
#[test]
fn install_refuses_a_copied_cli_that_is_not_the_images() {
    let env = Env::new("install-version");
    let vars = [("SHIM_NO_CONTAINER", "1"), ("SHIM_LATEST", "9.9.9")];
    let out = env.install(&["--from-image", "--no-init"], &vars);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("zephyr 9.9.9"), "{said}");
    find(
        &env.calls(),
        &["docker", "run", "--rm", "--entrypoint", "zephyr"],
    );
    assert!(local_bin(&env).is_empty(), "{:?}", local_bin(&env));
}

/// Arguments for `zephyr init` with `--no-init` would be dropped: a usage error instead.
#[test]
fn install_refuses_init_arguments_with_no_init() {
    let env = Env::new("install-no-init-args");
    let args = ["--no-init", "--", "--non-interactive"];
    let out = env.install(&args, &[("SHIM_NO_CONTAINER", "1")]);
    assert_eq!(out.status.code(), Some(2), "{out:?}");
    assert!(String::from_utf8_lossy(&out.stderr).contains("usage:"));
    let calls = env.calls();
    let went = calls
        .iter()
        .find(|c| c[0] == "cargo" || c.starts_with(&s(&["docker", "pull"])));
    assert!(went.is_none(), "{went:?}");
}

/// Installed from another repository: from source the CLI is built to upgrade from it; from the
/// image, whose CLI upgrades from where that image was built for, install.sh says to keep
/// ZEPHYR_REPO set.
#[test]
fn install_from_another_repo_says_how_upgrade_finds_it() {
    let repo = "registry.example.com/me/zephyr";
    let env = Env::new("install-repo-source");
    let vars = [("SHIM_NO_CONTAINER", "1"), ("ZEPHYR_REPO", repo)];
    let out = env.install(&["--from-source", "--no-init"], &vars);
    assert!(out.status.success(), "{out:?}");
    let baked = read(&env.dir.join("cargo-env.log"));
    assert_eq!(baked, format!("ZEPHYR_BUILD_REPO={repo}\n"));

    let env = Env::new("install-repo-image");
    let vars = [
        ("SHIM_NO_CONTAINER", "1"),
        ("SHIM_LATEST", "latest"),
        ("ZEPHYR_REPO", repo),
    ];
    let out = env.install(&["--from-image", "--no-init"], &vars);
    assert!(out.status.success(), "{out:?}");
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(
        said.contains(&format!("keep ZEPHYR_REPO={repo} set")),
        "{said}"
    );

    // The default repository needs neither.
    let env = Env::new("install-repo-default");
    let out = env.install(
        &["--from-source", "--no-init"],
        &[("SHIM_NO_CONTAINER", "1")],
    );
    assert!(out.status.success(), "{out:?}");
    let baked = read(&env.dir.join("cargo-env.log"));
    assert_eq!(baked, "ZEPHYR_BUILD_REPO=unset\n");
    assert!(!String::from_utf8_lossy(&out.stderr).contains("keep ZEPHYR_REPO"));
}

#[test]
fn install_from_source_runs_cargo_install() {
    let env = Env::new("install-source");
    let out = env.install(
        &["--from-source", "--no-init"],
        &[("SHIM_NO_CONTAINER", "1")],
    );
    assert!(out.status.success(), "{out:?}");
    let calls = env.calls();
    find(
        &calls,
        &["cargo", "install", "--locked", "--path", "crates/zephyr"],
    );
    find(&calls, &["docker", "buildx", "version"]);
    let pulled = calls.iter().any(|c| c.starts_with(&s(&["docker", "pull"])));
    assert!(!pulled, "{calls:#?}");
    fetches_nothing(&calls);
    assert!(env.home.join(".cargo/bin/zephyr").exists());
    assert!(local_bin(&env).is_empty(), "{:?}", local_bin(&env));
    assert!(!env.dir.join("exec.log").exists(), "init did not run");
}

#[test]
fn install_without_docker_names_its_install_page() {
    let env = Env::new("install-no-docker");
    let out = env.install(&[], &[("SHIM_DOCKER_DOWN", "1")]);
    assert_eq!(out.status.code(), Some(1), "{out:?}");
    let said = format!("{}{}", stdout(&out), String::from_utf8_lossy(&out.stderr));
    assert!(
        said.contains("https://docs.docker.com/engine/install/"),
        "{said}"
    );
    let calls = env.calls();
    assert!(!calls.iter().any(|c| c[0] == "cargo"), "{calls:#?}");
    fetches_nothing(&calls);
}

#[test]
fn install_sh_defaults_to_the_clis_repo() {
    let script = read(Path::new(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../deploy/install.sh"
    )));
    let default = format!("${{ZEPHYR_REPO:-{}}}", zephyr::host::DEFAULT_REPO);
    assert!(script.contains(&default), "{default}");
}
