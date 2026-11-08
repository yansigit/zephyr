//! The browser wrapper (approvals spec §4.4). `agent-browser` on the agent's PATH is
//! `/opt/zephyr/gate/bin/agent-browser`, a root-owned link to the zephyr binary, which acts as
//! this wrapper when `argv[0]` is `agent-browser` (main.rs, before any config is read: the agent
//! cannot read the daemon's). The real binary lives out of the PATH at
//! `/opt/zephyr/libexec/agent-browser` (`$ZEPHYR_AGENT_BROWSER` names another: tests' fake).
//!
//! Every command goes straight through (`exec`, as it was), except:
//!
//! - a click (`click`, `dblclick`, `tap`, `find … click`) and Enter (`press`/`key`/`keydown`
//!   Enter, or a line break typed with `type` or `keyboard type`): the wrapper asks the real
//!   binary for the page's address (`get url`) and for the control, its role and accessible name
//!   (`snapshot`'s refs, scoped to a CSS selector when one names it; for a key press the focused
//!   element, which a read-only script reports), classes it with `policy::browser_class`, and
//!   takes a message, post or payment, or a control it could not read, to the gate socket;
//! - typing (`fill`, `type`, `keyboard type|inserttext`, `find … fill`): its value goes to the
//!   gate first, for the private facts (never gated by class);
//! - `batch` carrying any of these or reading its commands from stdin, and agent-browser's own
//!   agents (`mcp`, `chat`), which would act past the wrapper: refused.
//!
//! The wrapper then waits for the gate's verdict with its connection open (A7: a closed one is the
//! action dropped, so a killed wrapper leaves nothing waiting). `allow` runs the agent's command
//! as it was; anything else, a closed socket or no gate at all exits 1 with the daemon's words,
//! or [`NOT_ANSWERING`], on stderr: what the agent reads. Nothing touches the page while it waits.
//!
//! What is not gated, by design: an unrecognized label (documented limit), `eval`, `mouse`,
//! `select`, `drag` and the rest of the commands (spec §4.4's list), and the real binary run by
//! its path or by a copy (a name check, like the plugin's: spec §2 non-goals).

use std::cell::OnceCell;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use zephyr_policy::policy::{self, Class};

use crate::gate::PageAmount;

/// Names the real agent-browser for the wrapper (`gate::agent_env` sets it; tests name a fake).
pub const REAL_ENV: &str = "ZEPHYR_AGENT_BROWSER";
/// Where the image keeps the real agent-browser, out of the agent's PATH.
pub const REAL: &str = "/opt/zephyr/libexec/agent-browser";
/// The root-owned folder whose `agent-browser` is this wrapper, first on the agent's PATH.
pub const GATE_BIN: &str = "/opt/zephyr/gate/bin";
/// What the agent reads when the gate cannot be asked or does not answer.
pub const NOT_ANSWERING: &str = "the gate is not answering; not run";

const BATCH: &str = "not run: agent-browser batch takes no clicks, key presses or typing here, \
                     which the approvals gate checks one by one; run each as a command of its own";
const BATCH_STDIN: &str = "not run: give agent-browser batch its commands as arguments here, so \
                           the approvals gate can check them";
const AGENTS: &str = "not run: agent-browser's mcp and chat act on the page past the approvals \
                      gate; use its commands";

/// The longest a verdict takes: a browser wait's 10 minutes and the tick that ends them.
const VERDICT_WAIT: Duration = Duration::from_secs(60 * crate::gate::WAIT_MINUTES as u64 + 60);
/// How long one question to the real binary may take, as the vault's fill allows.
const FACT_WAIT: Duration = Duration::from_secs(20);
/// The most of the real binary's answer read (a big page's snapshot).
const MAX_OUTPUT: u64 = 32 << 20;
/// What the real binary's own value is, of a typed value, at most (a composer's text).
const MAX_TYPED: usize = 4000;

/// agent-browser's global flags that take no value (or an optional `true`/`false`), and those
/// that take one, wherever they stand (its `clean_args`, 0.41).
const BOOL_FLAGS: &[&str] = &[
    "--json",
    "--headed",
    "--webgpu",
    "--no-webmcp",
    "--debug",
    "--ignore-https-errors",
    "--allow-file-access",
    "--hide-scrollbars",
    "--auto-connect",
    "--pin-tab",
    "--no-pin-tab",
    "--no-ca-cert",
    "--use-system-ca",
    "--annotate",
    "--content-boundaries",
    "--confirm-interactive",
    "--no-auto-dialog",
    "-v",
    "--verbose",
    "-q",
    "--quiet",
    "--offline",
    "--quick",
    "--fix",
];
const VALUE_FLAGS: &[&str] = &[
    "--session",
    "--restore-save",
    "--restore-check-url",
    "--restore-check-text",
    "--restore-check-fn",
    "--namespace",
    "--headers",
    "--executable-path",
    "--cdp",
    "--extension",
    "--init-script",
    "--enable",
    "--profile",
    "--state",
    "--proxy",
    "--proxy-bypass",
    "--args",
    "--user-agent",
    "-p",
    "--provider",
    "--device",
    "--session-name",
    "--color-scheme",
    "--download-path",
    "--max-output",
    "--allowed-domains",
    "--action-policy",
    "--confirm-actions",
    "--config",
    "--engine",
    "--input-mode",
    "--screenshot-dir",
    "--screenshot-quality",
    "--screenshot-format",
    "--idle-timeout",
    "--ca-cert",
    "--model",
];
/// Global flags that shape the output, which the wrapper's own questions leave out: everything
/// else (the session, the browser it connects to) goes with them, so they read the agent's page.
const SHAPING: &[&str] = &[
    "--json",
    "--max-output",
    "--content-boundaries",
    "--annotate",
    "-v",
    "--verbose",
    "-q",
    "--quiet",
    "--debug",
];
/// agent-browser's top-level commands (`is_top_level_command`): a bare `--restore` takes no value
/// before one.
const COMMANDS: &[&str] = &[
    "open",
    "goto",
    "navigate",
    "back",
    "forward",
    "reload",
    "read",
    "click",
    "dblclick",
    "fill",
    "type",
    "hover",
    "focus",
    "check",
    "uncheck",
    "select",
    "drag",
    "upload",
    "download",
    "press",
    "key",
    "keydown",
    "keyup",
    "keyboard",
    "scroll",
    "scrollintoview",
    "scrollinto",
    "wait",
    "screenshot",
    "pdf",
    "snapshot",
    "eval",
    "close",
    "quit",
    "exit",
    "inspect",
    "auth",
    "confirm",
    "deny",
    "connect",
    "stream",
    "get",
    "is",
    "find",
    "mouse",
    "set",
    "network",
    "storage",
    "cookies",
    "tab",
    "window",
    "frame",
    "dialog",
    "trace",
    "profiler",
    "record",
    "console",
    "errors",
    "highlight",
    "clipboard",
    "state",
    "tap",
    "swipe",
    "device",
    "diff",
    "batch",
    "react",
    "vitals",
    "web-vitals",
    "a11y",
    "pushstate",
    "removeinitscript",
    "session",
    "mcp",
    "doctor",
    "install",
    "upgrade",
    "profiles",
    "skills",
    "dashboard",
    "plugin",
    "plugins",
    "chat",
    "webmcp",
];

/// Where the wrapper runs: the real binary, the gate socket (`ZEPHYR_GATE_SOCKET`) and the
/// session its requests name (`ZEPHYR_SESSION`), as every process run for the agent has them.
#[derive(Clone, Debug)]
pub struct Env {
    pub real: PathBuf,
    pub socket: Option<PathBuf>,
    pub session: String,
}

impl Env {
    pub fn from_process() -> Env {
        use zephyr_policy::tools::{GATE_SOCKET_ENV, SESSION_ENV};
        let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
        Env {
            real: var(REAL_ENV).map_or_else(|| PathBuf::from(REAL), PathBuf::from),
            socket: var(GATE_SOCKET_ENV).map(PathBuf::from),
            session: std::env::var(SESSION_ENV).unwrap_or_default(),
        }
    }

    /// The real binary is named by its full path and is not this wrapper: a bare name would be
    /// looked up on the PATH, where `agent-browser` is the wrapper itself.
    pub fn check_real(&self) -> Result<(), String> {
        if !self.real.is_absolute() {
            return Err(format!(
                "{REAL_ENV} must name the real agent-browser by its full path, not {}",
                self.real.display()
            ));
        }
        let me = std::fs::canonicalize("/proc/self/exe");
        if let (Ok(me), Ok(real)) = (me, std::fs::canonicalize(&self.real)) {
            if me == real {
                return Err(format!(
                    "{} is this wrapper, not the real agent-browser",
                    self.real.display()
                ));
            }
        }
        Ok(())
    }
}

/// What the wrapper does with the agent's command.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Next {
    /// Runs it as it was.
    Exec,
    /// Runs nothing; the text is for the agent.
    Refuse(String),
}

/// `agent-browser ARGS` as the agent runs it: decided, then the real binary run in this process's
/// place, or refused with exit 1.
pub fn main(args: Vec<String>) -> ExitCode {
    let env = Env::from_process();
    if let Err(e) = env.check_real() {
        eprintln!("agent-browser: {e}");
        return ExitCode::from(127);
    }
    match run(&args, &env) {
        Next::Exec => {
            use std::os::unix::process::CommandExt;
            let e = Command::new(&env.real).args(&args).exec();
            eprintln!("agent-browser: running {}: {e}", env.real.display());
            ExitCode::from(127)
        }
        Next::Refuse(text) => {
            eprintln!("{text}");
            ExitCode::FAILURE
        }
    }
}

/// Decides `args` (the agent's argv after `agent-browser`), asking the real binary about the page
/// and the gate about the action as needed. Never runs the action itself.
pub fn run(args: &[String], env: &Env) -> Next {
    // agent-browser prints its help or its version for these, wherever they stand (a flag's value
    // too), and does nothing else (its `main`, 0.41). Not inside a batch: there it runs each
    // command as it parses it, so a batch is read without this.
    let only_says = |a: &String| matches!(a.as_str(), "--help" | "-h" | "--version" | "-V");
    if args.iter().any(only_says) {
        return Next::Exec;
    }
    let argv = split(args);
    let page = Page {
        env,
        flags: forwarded(&argv.globals),
        snap: OnceCell::new(),
    };
    match what(&argv.rest) {
        Act::Pass => Next::Exec,
        Act::Refuse(text) => Next::Refuse(text.into()),
        Act::Type { action, typed } => {
            let seen = Seen {
                typed: vec![typed],
                ..Seen::default()
            };
            ask_gate(env, args, &browser(action, &page.url(), &seen))
        }
        Act::Click(target) => {
            let url = page.url();
            let mut seen = page.control(&target);
            if plain_submit(&seen) {
                // A `submit` posts on a form with a textarea: unread, it counts as one.
                seen.textarea = match page.eval(SUBMIT_JS) {
                    Some(v) if v["found"].as_u64().is_some_and(|n| n > 0) => v["textarea"] == true,
                    _ => true,
                };
            }
            decide(env, args, &page, "click", &url, seen)
        }
        Act::Key { target, typed } => {
            let url = page.url();
            let mut seen = match &target {
                Target::Focus => page.focused(),
                target => {
                    let mut seen = page.control(target);
                    seen.composer = matches!(seen.role.as_str(), "textbox" | "searchbox");
                    seen
                }
            };
            seen.typed.extend(typed);
            decide(env, args, &page, "press", &url, seen)
        }
    }
}

/// A click or a key press on `seen`: through at once when the control is read and is no message,
/// post or payment; otherwise the gate decides (a payment with the page's total and title).
fn decide(
    env: &Env,
    args: &[String],
    page: &Page,
    action: &str,
    url: &str,
    mut seen: Seen,
) -> Next {
    let class = seen.class();
    if class.is_none() && !seen.role.trim().is_empty() {
        return Next::Exec;
    }
    if class == Some(Class::Money) {
        seen.amount = page.snap().and_then(|s| page_total(&s.tree));
        seen.title = page.title();
    }
    ask_gate(env, args, &browser(action, url, &seen))
}

/// The gate's verdict on the request (spec §4.2): `allow` runs the command; `hold` and `deny`
/// refuse it with the daemon's text; no gate, a closed socket or an answer that cannot be read
/// refuse it with [`NOT_ANSWERING`]. The connection stays open until the answer (A7).
fn ask_gate(env: &Env, args: &[String], browser: &Value) -> Next {
    let Some(socket) = &env.socket else {
        return Next::Refuse(NOT_ANSWERING.into());
    };
    let req = json!({
        "v": 1,
        "session": env.session,
        "tool": "browser",
        "args": { "argv": args },
        "browser": browser,
    });
    match zephyr_policy::gate_client::ask(socket, &req, VERDICT_WAIT) {
        Ok(a) if a["verdict"] == "allow" => Next::Exec,
        Ok(a) if matches!(a["verdict"].as_str(), Some("hold" | "deny")) => {
            let text = a["text"].as_str().filter(|t| !t.trim().is_empty());
            Next::Refuse(text.unwrap_or("not run: the gate did not allow it").into())
        }
        _ => Next::Refuse(NOT_ANSWERING.into()),
    }
}

/// The request's `browser` part (`gate::Browser`).
fn browser(action: &str, url: &str, seen: &Seen) -> Value {
    let origin = crate::vault::origin(url).map_or_else(String::new, |o| o.ascii_serialization());
    json!({
        "action": action,
        "url": url,
        "origin": origin,
        "role": seen.role,
        "name": seen.name,
        "typed": seen.typed,
        "form_textarea": seen.textarea,
        "form_password": seen.password,
        "composer": seen.composer,
        "amount": seen.amount,
        "title": seen.title,
    })
}

// ---- the agent's command ----

/// The agent's argv as agent-browser reads it: the global flags, anywhere, each with its value,
/// and the rest, the command first.
struct Argv {
    globals: Vec<Vec<String>>,
    rest: Vec<String>,
}

fn split(args: &[String]) -> Argv {
    let (mut globals, mut rest) = (Vec::new(), Vec::new());
    let mut seen_command = false;
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        let next = args.get(i + 1);
        let takes = match a.as_str() {
            _ if a.starts_with("--restore=") => false,
            f if VALUE_FLAGS.contains(&f) => next.is_some(),
            "--restore" => next.is_some_and(|v| {
                !seen_command && !v.starts_with('-') && !COMMANDS.contains(&v.as_str())
            }),
            f if BOOL_FLAGS.contains(&f) => {
                next.is_some_and(|v| matches!(v.as_str(), "true" | "false"))
            }
            _ => {
                if !a.starts_with('-') && COMMANDS.contains(&a.as_str()) {
                    seen_command = true;
                }
                rest.push(a.clone());
                i += 1;
                continue;
            }
        };
        let mut flag = vec![a.clone()];
        if takes {
            flag.extend(next.cloned());
            i += 1;
        }
        globals.push(flag);
        i += 1;
    }
    Argv { globals, rest }
}

/// The agent's global flags for the wrapper's own questions: all but those that shape output.
fn forwarded(globals: &[Vec<String>]) -> Vec<String> {
    (globals.iter())
        .filter(|f| !SHAPING.contains(&f[0].as_str()))
        .flatten()
        .cloned()
        .collect()
}

/// What the command does, as the wrapper sees it. No `Debug`: what is typed can hold a private
/// fact.
#[derive(PartialEq)]
enum Act {
    Pass,
    Refuse(&'static str),
    /// A click on the target.
    Click(Target),
    /// Enter in the focused element, or in the target a line break is typed into.
    Key {
        target: Target,
        typed: Vec<String>,
    },
    /// Typing a value (`fill`, `type`).
    Type {
        action: &'static str,
        typed: String,
    },
}

/// The control an action is on.
#[derive(Debug, PartialEq)]
enum Target {
    /// A ref (`@e3`), a CSS selector, `text=…` or `xpath=…`.
    Selector(String),
    /// A `find` locator: `role`, `text`, `label`, `placeholder`, `alt`, `title`, `testid`,
    /// `first`, `last`, `nth`, with its value, its `--name` and `--exact`.
    Find {
        by: String,
        value: String,
        name: Option<String>,
        exact: bool,
    },
    /// The focused element.
    Focus,
}

fn what(rest: &[String]) -> Act {
    let Some((cmd, args)) = rest.split_first() else {
        return Act::Pass;
    };
    let text = |parts: &[String]| parts.join(" ");
    match cmd.as_str() {
        "click" | "dblclick" | "tap" => {
            let sel = args
                .iter()
                .find(|a| !matches!(a.as_str(), "--new-tab" | "--human"));
            sel.map_or(Act::Pass, |s| Act::Click(Target::Selector(s.clone())))
        }
        "press" | "key" | "keydown" => match args.first() {
            Some(key) if enter(key) => Act::Key {
                target: Target::Focus,
                typed: Vec::new(),
            },
            _ => Act::Pass,
        },
        "keyboard" => match args.split_first() {
            Some((sub, words)) if matches!(sub.as_str(), "type" | "inserttext" | "insertText") => {
                typing(
                    "type",
                    text(words),
                    (sub == "type").then_some(Target::Focus),
                )
            }
            _ => Act::Pass,
        },
        "fill" => match args.split_first() {
            Some((_, words)) => typing("fill", text(words), None),
            None => Act::Pass,
        },
        "type" => {
            let Some((sel, more)) = args.split_first() else {
                return Act::Pass;
            };
            let (mut words, mut i) = (Vec::new(), 0);
            while i < more.len() {
                match more[i].as_str() {
                    "--clear" => {}
                    "--delay" => i += 1,
                    w => words.push(w.to_string()),
                }
                i += 1;
            }
            typing("type", words.join(" "), Some(Target::Selector(sel.clone())))
        }
        "find" => find(args),
        "batch" => batch(args),
        "mcp" | "chat" => Act::Refuse(AGENTS),
        _ => Act::Pass,
    }
}

/// Typing `typed`: a facts check, or with a line break where Enter would go (`enter_in`), a key
/// press there. Nothing typed is nothing to check.
fn typing(action: &'static str, typed: String, enter_in: Option<Target>) -> Act {
    if typed.trim().is_empty() {
        return Act::Pass;
    }
    match enter_in {
        Some(target) if typed.contains(['\n', '\r']) => Act::Key {
            target,
            typed: vec![typed],
        },
        _ => Act::Type { action, typed },
    }
}

/// `Enter`, `Control+Enter`, `NumpadEnter`, `Return`: a key that sends.
fn enter(key: &str) -> bool {
    let last = key.rsplit('+').next().unwrap_or(key).to_ascii_lowercase();
    matches!(last.as_str(), "enter" | "return" | "numpadenter")
}

/// `find LOCATOR VALUE [ACTION] [TEXT] [--name N] [--exact]`, `find nth N SELECTOR [ACTION]
/// [TEXT]`, as agent-browser reads them; the action is `click` when none is named.
fn find(args: &[String]) -> Act {
    let Some(by) = args.first() else {
        return Act::Pass;
    };
    let (target, action, fill) = if by == "nth" {
        let Some(sel) = args.get(2) else {
            return Act::Pass;
        };
        let target = Target::Find {
            by: by.clone(),
            value: sel.clone(),
            name: None,
            exact: false,
        };
        let fill = args.get(4..).unwrap_or_default().join(" ");
        (target, args.get(3).map_or("click", String::as_str), fill)
    } else {
        let Some(value) = args.get(1) else {
            return Act::Pass;
        };
        let (mut name, mut exact, mut parts) = (None, false, Vec::new());
        let mut i = 3;
        while i < args.len() {
            match args[i].as_str() {
                "--exact" => exact = true,
                "--name" => {
                    name = args.get(i + 1).cloned();
                    i += 1;
                }
                w => parts.push(w.to_string()),
            }
            i += 1;
        }
        let target = Target::Find {
            by: by.clone(),
            value: value.clone(),
            name,
            exact,
        };
        (
            target,
            args.get(2).map_or("click", String::as_str),
            parts.join(" "),
        )
    };
    match action {
        "click" => Act::Click(target),
        "fill" => typing("fill", fill, None),
        _ => Act::Pass,
    }
}

/// A batch passes only when none of its commands would need the gate, read as the wrapper reads a
/// command; one read from stdin cannot be seen first, so it is refused.
fn batch(args: &[String]) -> Act {
    let commands: Vec<&String> = (args.iter())
        .filter(|a| !matches!(a.as_str(), "--bail" | "--omit-command"))
        .collect();
    if commands.is_empty() {
        return Act::Refuse(BATCH_STDIN);
    }
    for c in commands {
        let words: Vec<String> = match c.trim_start().starts_with('[') {
            true => match serde_json::from_str(c) {
                Ok(words) => words,
                Err(_) => return Act::Refuse(BATCH),
            },
            false => (c.split_whitespace())
                .map(|w| w.trim_matches(['"', '\'']).to_string())
                .collect(),
        };
        if what(&split(&words).rest) != Act::Pass {
            return Act::Refuse(BATCH);
        }
    }
    Act::Pass
}

// ---- the page, read through the real binary ----

/// What the wrapper read of the control: what `gate::Browser` carries. No `Debug`: what is
/// typed can hold a private fact.
#[derive(Default)]
struct Seen {
    role: String,
    name: String,
    textarea: bool,
    password: bool,
    composer: bool,
    typed: Vec<String>,
    amount: Option<PageAmount>,
    title: String,
}

impl Seen {
    fn class(&self) -> Option<Class> {
        policy::browser_class(
            &self.role,
            &self.name,
            self.textarea,
            self.password,
            self.composer,
        )
    }
}

/// A button named only `submit`: a post on a form with a textarea.
fn plain_submit(seen: &Seen) -> bool {
    let name = seen.name.to_lowercase();
    let words: Vec<&str> = (name.split(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .collect();
    words == ["submit"] && matches!(seen.role.as_str(), "button" | "menuitem")
}

/// A control as a snapshot names it.
#[derive(Clone, Debug, Default, PartialEq)]
struct Control {
    role: String,
    name: String,
}

impl Control {
    fn seen(&self) -> Seen {
        Seen {
            role: self.role.clone(),
            name: self.name.clone(),
            ..Seen::default()
        }
    }
}

/// A snapshot: its refs (each control's role and name) and its text.
struct Snap {
    refs: Vec<(String, Control)>,
    tree: String,
}

impl Snap {
    fn of(data: &Value) -> Option<Snap> {
        let tree = data["snapshot"].as_str()?.to_string();
        let mut refs: Vec<(String, Control)> = (data["refs"].as_object()?.iter())
            .map(|(r, c)| {
                let s = |k: &str| c[k].as_str().unwrap_or_default().to_string();
                let control = Control {
                    role: s("role"),
                    name: s("name"),
                };
                (r.clone(), control)
            })
            .collect();
        let number = |r: &str| r.trim_start_matches('e').parse::<u64>().unwrap_or(u64::MAX);
        refs.sort_by_key(|(r, _)| number(r));
        Some(Snap { refs, tree })
    }

    /// The controls at the top of the tree: what a snapshot scoped to a selector matched.
    fn top(&self) -> Vec<Control> {
        let lines: Vec<Line> = self.tree.lines().filter_map(line).collect();
        let depth = lines.iter().map(|l| l.depth).min();
        (lines.into_iter())
            .filter(|l| Some(l.depth) == depth)
            .map(|l| Control {
                role: l.role,
                name: l.name,
            })
            .collect()
    }
}

/// The real binary, asked as the agent's own command would be (its session, its browser).
struct Page<'a> {
    env: &'a Env,
    flags: Vec<String>,
    snap: OnceCell<Option<Snap>>,
}

impl Page<'_> {
    /// `agent-browser FLAGS ARGS --json`: the answer's `data`, `None` when it fails, says it did
    /// not succeed, or takes longer than [`FACT_WAIT`].
    fn ask(&self, args: &[&str]) -> Option<Value> {
        use std::io::Read;
        let mut child = Command::new(&self.env.real)
            .args(&self.flags)
            .args(args)
            .arg("--json")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let mut out = child.stdout.take()?;
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = (&mut out).take(MAX_OUTPUT).read_to_end(&mut buf);
            let _ = tx.send(buf);
        });
        let deadline = Instant::now() + FACT_WAIT;
        let left = || deadline.saturating_duration_since(Instant::now());
        // Read to the end (a grandchild holding the pipe open is waited for no longer than this).
        let buf = rx.recv_timeout(left()).ok();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if !left().is_zero() => std::thread::sleep(Duration::from_millis(10)),
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        let text = String::from_utf8_lossy(&buf?).into_owned();
        let answer = (text.lines().rev())
            .filter_map(|l| serde_json::from_str::<Value>(l.trim()).ok())
            .find(|v| v.get("success").is_some())?;
        (answer["success"] == true).then(|| answer["data"].clone())
    }

    /// The page's address: empty when it cannot be read (its origin is then `unknown`).
    fn url(&self) -> String {
        let data = self.ask(&["get", "url"]);
        let url = data.as_ref().and_then(|d| d["url"].as_str());
        url.unwrap_or_default().trim().to_string()
    }

    fn title(&self) -> String {
        let data = self.ask(&["get", "title"]);
        let title = data.as_ref().and_then(|d| d["title"].as_str());
        title.unwrap_or_default().to_string()
    }

    /// The page's snapshot, taken once. It keeps the agent's refs: they belong to the elements,
    /// and a snapshot gives each element the ref it had.
    fn snap(&self) -> Option<&Snap> {
        let taken = self
            .snap
            .get_or_init(|| Snap::of(&self.ask(&["snapshot"])?));
        taken.as_ref()
    }

    /// A read-only script's answer, its JSON text read back.
    fn eval(&self, script: &str) -> Option<Value> {
        let encoded = base64::engine::general_purpose::STANDARD.encode(script);
        let data = self.ask(&["eval", "-b", &encoded])?;
        match &data["result"] {
            Value::String(s) => serde_json::from_str(s).ok(),
            v @ Value::Object(_) => Some(v.clone()),
            _ => None,
        }
    }

    /// The control `target` names. Nothing readable (no such ref, a failed snapshot) is a control
    /// with neither role nor name, which the gate holds; a locator whose text the page has
    /// nowhere keeps that text as the name, for its class (R39).
    fn control(&self, target: &Target) -> Seen {
        match target {
            Target::Focus => self.focused(),
            Target::Selector(sel) => {
                if let Some(r) = ref_of(sel) {
                    let snap = self.snap();
                    let found = snap.and_then(|s| s.refs.iter().find(|(id, _)| id == r));
                    return found.map_or_else(Seen::default, |(_, c)| c.seen());
                }
                if let Some(text) = sel.strip_prefix("text=") {
                    return self.named(text, false);
                }
                if sel.starts_with("xpath=") || sel.starts_with('/') {
                    return Seen::default();
                }
                self.scoped(sel)
            }
            Target::Find {
                by,
                value,
                name,
                exact,
            } => match by.as_str() {
                "role" => {
                    let Some(snap) = self.snap() else {
                        return Seen::default();
                    };
                    let fits = |c: &Control| {
                        c.role.eq_ignore_ascii_case(value)
                            && name
                                .as_deref()
                                .is_none_or(|n| matches_text(&c.name, n, *exact))
                    };
                    let all = snap.refs.iter().map(|(_, c)| c).filter(|c| fits(c));
                    strictest(all.cloned().collect()).map_or_else(Seen::default, |c| c.seen())
                }
                "text" | "label" | "placeholder" | "alt" | "title" => self.named(value, *exact),
                "testid" => self.scoped(&format!("[data-testid={}]", Value::from(value.as_str()))),
                _ => self.scoped(value), // first, last, nth: a CSS selector
            },
        }
    }

    /// The strictest control whose name holds `text` (all of it with `exact`); none is `text`
    /// itself, with no role.
    fn named(&self, text: &str, exact: bool) -> Seen {
        let Some(snap) = self.snap() else {
            return Seen::default();
        };
        let all = snap.refs.iter().map(|(_, c)| c);
        let found: Vec<Control> = all
            .filter(|c| matches_text(&c.name, text, exact))
            .cloned()
            .collect();
        match strictest(found) {
            Some(c) => c.seen(),
            None => Seen {
                name: text.to_string(),
                ..Seen::default()
            },
        }
    }

    /// The strictest control a CSS selector matches, read from a snapshot scoped to it.
    fn scoped(&self, selector: &str) -> Seen {
        let scoped = self.ask(&["snapshot", "-s", selector]);
        let top = scoped.as_ref().and_then(Snap::of).map(|s| s.top());
        strictest(top.unwrap_or_default()).map_or_else(Seen::default, |c| c.seen())
    }

    /// The focused element, read by a script (a snapshot does not mark focus): its role and name
    /// with its placeholder folded in (a search box named only by its placeholder stays one), its
    /// text (what Enter sends, checked for facts and shown on the card) and its form.
    fn focused(&self) -> Seen {
        let Some(v) = self.eval(FOCUS_JS) else {
            return Seen::default();
        };
        if v["unread"] == true {
            return Seen::default();
        }
        let s = |k: &str| v[k].as_str().unwrap_or_default().trim().to_string();
        let (role, name, placeholder) = (s("role"), s("name"), s("placeholder"));
        let name = match placeholder.is_empty()
            || name.to_lowercase().contains(&placeholder.to_lowercase())
        {
            true => name,
            false => format!("{name} {placeholder}").trim().to_string(),
        };
        let value = v["value"].as_str().unwrap_or_default();
        let value: String = value.chars().take(MAX_TYPED).collect();
        Seen {
            composer: v["editable"] == true || role == "textbox",
            role,
            name,
            textarea: v["textarea"] == true,
            password: v["password"] == true,
            typed: (!value.trim().is_empty())
                .then_some(value)
                .into_iter()
                .collect(),
            ..Seen::default()
        }
    }
}

/// `e3` for `@e3`, `e3` or `ref=e3`.
fn ref_of(sel: &str) -> Option<&str> {
    let r = sel
        .strip_prefix('@')
        .or_else(|| sel.strip_prefix("ref="))
        .unwrap_or(sel);
    let digits = r.strip_prefix('e')?;
    (!digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())).then_some(r)
}

/// Whether `name` holds `text`, as agent-browser's locators match: case-insensitively, as a part,
/// or (`exact`) whole.
fn matches_text(name: &str, text: &str, exact: bool) -> bool {
    let (name, text) = (name.trim(), text.trim());
    match exact {
        true => name == text,
        false => !text.is_empty() && name.to_lowercase().contains(&text.to_lowercase()),
    }
}

/// Of the controls an action may land on, the one whose class is strictest (a payment, a post, a
/// message), else the first.
fn strictest(controls: Vec<Control>) -> Option<Control> {
    let rank = |c: &Control| match policy::browser_class(&c.role, &c.name, false, false, false) {
        Some(Class::Money) => 3,
        Some(Class::Posts) => 2,
        Some(Class::Messages) => 1,
        None => 0,
    };
    let ranked = controls.into_iter().enumerate();
    let best = ranked.max_by_key(|(i, c)| (rank(c), std::cmp::Reverse(*i)));
    best.map(|(_, c)| c)
}

/// The focused element's facts, as JSON text: `{unread}` inside another site's frame, `{role:
/// "document"}` when nothing is focused. Reads the page only.
const FOCUS_JS: &str = r#"(() => {
  let e = document.activeElement;
  for (let i = 0; e && i < 16; i++) {
    if (e.shadowRoot && e.shadowRoot.activeElement) { e = e.shadowRoot.activeElement; continue; }
    if (e.tagName === 'IFRAME' || e.tagName === 'FRAME') {
      let d = null;
      try { d = e.contentDocument; } catch (_) {}
      if (!d) return JSON.stringify({ unread: true });
      e = d.activeElement;
      continue;
    }
    break;
  }
  if (!e) return JSON.stringify({ unread: true });
  const doc = e.ownerDocument;
  if (e === doc.body || e === doc.documentElement) return JSON.stringify({ role: 'document', name: '' });
  const tag = e.tagName.toLowerCase();
  const type = (e.getAttribute('type') || '').toLowerCase();
  const inputs = { search: 'searchbox', button: 'button', submit: 'button', reset: 'button',
    image: 'button', checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton' };
  let role = (e.getAttribute('role') || '').trim().split(/\s+/)[0];
  if (!role) {
    if (tag === 'textarea') role = 'textbox';
    else if (tag === 'input') role = inputs[type] || 'textbox';
    else if (tag === 'button') role = 'button';
    else if (tag === 'a' && e.hasAttribute('href')) role = 'link';
    else if (tag === 'select') role = 'combobox';
    else if (e.isContentEditable) role = 'textbox';
    else role = 'generic';
  }
  const text = n => ((n && (n.innerText || n.textContent)) || '').trim();
  const by = (e.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
    .map(id => text(doc.getElementById(id))).join(' ').trim();
  const labels = e.labels ? Array.from(e.labels).map(text).join(' ').trim() : '';
  let name = (e.getAttribute('aria-label') || '').trim() || by || labels;
  if (!name && (role === 'button' || role === 'link' || role === 'menuitem')) name = text(e) || e.value || '';
  if (!name) name = e.getAttribute('title') || e.getAttribute('alt') || '';
  const field = tag === 'textarea' || (tag === 'input' && (!(type in inputs) || type === 'search'));
  // A password's value is never read: the vault's fill handles logins.
  const value = e.isContentEditable ? text(e) : field && type !== 'password' ? (e.value || '') : '';
  const form = e.form || (e.closest && e.closest('form'));
  return JSON.stringify({
    role, name: String(name).trim(),
    placeholder: e.getAttribute('placeholder') || e.getAttribute('aria-placeholder') || '',
    editable: !!e.isContentEditable || field, value: String(value).slice(0, 4000),
    textarea: !!(form && form.querySelector('textarea, [contenteditable]:not([contenteditable="false"])')),
    password: !!(form && form.querySelector('input[type="password" i]')),
  });
})()"#;

/// Whether a form holding a control named `submit` has a textarea (or an editable area). Reads the
/// page only.
const SUBMIT_JS: &str = r#"(() => {
  const norm = s => String(s || '').replace(/\s+/g, ' ').trim().toLowerCase();
  const named = e => norm(e.getAttribute('aria-label') || e.innerText || e.value || e.getAttribute('title'));
  const all = Array.from(document.querySelectorAll('button, input[type="submit" i], input[type="button" i], ' +
    'input[type="image" i], [role="button"], [role="menuitem"]')).filter(e => named(e) === 'submit');
  const forms = all.map(e => e.form || e.closest('form'));
  const area = 'textarea, [contenteditable]:not([contenteditable="false"])';
  return JSON.stringify({ found: all.length, textarea: forms.some(f => !!(f && f.querySelector(area))) });
})()"#;

// ---- the snapshot's text ----

/// One line of agent-browser's snapshot (0.41): `- role "name" [level=1, ref=e3] …: value`,
/// two spaces of indent a level, the name written as JSON.
struct Line {
    depth: usize,
    role: String,
    name: String,
    value: String,
}

fn line(raw: &str) -> Option<Line> {
    let trimmed = raw.trim_start();
    let depth = (raw.len() - trimmed.len()) / 2;
    let rest = trimmed.strip_prefix("- ")?;
    let (role, mut rest) = rest.split_once(' ').unwrap_or((rest, ""));
    let mut name = String::new();
    if rest.starts_with('"') {
        let mut strings = serde_json::Deserializer::from_str(rest).into_iter::<String>();
        if let Some(Ok(s)) = strings.next() {
            name = s;
            rest = &rest[strings.byte_offset()..];
        }
    }
    let value = rest.split_once(": ").map_or("", |(_, v)| v);
    Some(Line {
        depth,
        role: role.trim_end_matches(':').to_string(),
        name,
        value: value.to_string(),
    })
}

/// The page's labelled total (`total`, `order total`, `amount due`, `you pay`; Korean `합계`,
/// `총액`, `결제금액`), read from its snapshot: the amount after the label, in its text or in
/// one of the three texts that follow. Of several, the largest, so a `total savings` never
/// stands for the order's total; amounts that name a currency count over those that name none,
/// and totals in two currencies are no total. `$` reads as US dollars (every other dollar is
/// worth less, so a cap is never passed on that reading); `¥` names no currency (yen or yuan).
pub fn page_total(tree: &str) -> Option<PageAmount> {
    let texts: Vec<String> = (tree.lines().filter_map(line))
        .flat_map(|l| [l.name, l.value])
        .filter(|t| !t.trim().is_empty())
        .collect();
    let mut found = Vec::new();
    for (i, t) in texts.iter().enumerate() {
        let Some(after) = after_label(t) else {
            continue;
        };
        let next = texts.get(i + 1..).unwrap_or_default().iter().take(3);
        let ahead: Vec<PageAmount> = next.filter_map(|n| money(n)).collect();
        let marked = ahead.iter().find(|a| !a.currency.is_empty()).cloned();
        found.extend(
            money(&after)
                .or(marked)
                .or_else(|| ahead.into_iter().next()),
        );
    }
    let marked: Vec<&PageAmount> = found.iter().filter(|a| !a.currency.is_empty()).collect();
    let pool = match marked.is_empty() {
        true => found.iter().collect(),
        false => marked,
    };
    let first = pool.first()?;
    if pool.iter().any(|a| a.currency != first.currency) {
        return None;
    }
    pool.into_iter()
        .max_by(|a, b| a.value.total_cmp(&b.value))
        .cloned()
}

/// What follows a total's label in `text`, lower-cased, or `None` when it has none.
fn after_label(text: &str) -> Option<String> {
    let lower = text.to_lowercase();
    const HANGUL: &[&str] = &["결제금액", "결제 금액", "총 금액", "총금액", "총액", "합계"];
    if let Some((i, k)) = HANGUL.iter().find_map(|k| lower.find(k).map(|i| (i, k))) {
        return Some(lower[i + k.len()..].to_string());
    }
    const LATIN: &[&str] = &[
        "amount due",
        "amount to pay",
        "balance due",
        "you pay",
        "total",
    ];
    LATIN
        .iter()
        .find_map(|k| word_at(&lower, k).map(|i| lower[i + k.len()..].to_string()))
}

/// Where `word` stands in `hay` as a whole word (no letter or digit beside it).
fn word_at(hay: &str, word: &str) -> Option<usize> {
    let mut from = 0;
    while let Some(i) = hay[from..].find(word).map(|i| i + from) {
        let clear_before = hay[..i]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_alphanumeric());
        let end = i + word.len();
        let clear_after = hay[end..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_alphanumeric());
        if clear_before && clear_after {
            return Some(i);
        }
        from = end;
    }
    None
}

/// The first amount in `text` that names its currency, else its first number.
fn money(text: &str) -> Option<PageAmount> {
    let all = amounts(text);
    let marked = all.iter().find(|a| !a.currency.is_empty()).cloned();
    marked.or_else(|| all.into_iter().next())
}

/// Every number in `text`, each with the currency written beside it (empty for none).
fn amounts(text: &str) -> Vec<PageAmount> {
    let chars: Vec<char> = text.chars().collect();
    let (mut out, mut i) = (Vec::new(), 0);
    while i < chars.len() {
        if !chars[i].is_ascii_digit() {
            i += 1;
            continue;
        }
        let start = i;
        while i < chars.len()
            && (chars[i].is_ascii_digit()
                || (matches!(chars[i], ',' | '.')
                    && chars.get(i + 1).is_some_and(char::is_ascii_digit)))
        {
            i += 1;
        }
        let number: String = chars[start..i].iter().collect();
        let before: String = chars[..start].iter().collect();
        let after: String = chars[i..].iter().collect();
        if let Some(value) = number_value(&number) {
            let currency = currency(before.trim_end(), after.trim_start());
            out.push(PageAmount { value, currency });
        }
    }
    out
}

/// `1,234.56`, `1.234,56`, `54,200`, `54.20`: the last separator is the decimal point when one or
/// two digits (or more than three) follow it; with exactly three it groups thousands, the larger
/// reading.
fn number_value(number: &str) -> Option<f64> {
    let plain = |s: &str| s.chars().filter(char::is_ascii_digit).collect::<String>();
    let text = match number.rfind([',', '.']) {
        Some(i) if number.len() - i - 1 != 3 => {
            format!("{}.{}", plain(&number[..i]), &number[i + 1..])
        }
        _ => plain(number),
    };
    text.parse::<f64>().ok().filter(|v| v.is_finite())
}

/// ISO 4217 codes read beside an amount.
const CODES: &[&str] = &[
    "USD", "EUR", "GBP", "JPY", "CNY", "KRW", "CAD", "AUD", "NZD", "CHF", "HKD", "SGD", "INR",
    "BRL", "MXN", "SEK", "NOK", "DKK", "PLN", "CZK", "TRY", "ZAR", "TWD", "THB",
];

/// The currency written right before (`$`, `US$`, `€`, `EUR`) or right after (`원`, `€`, `EUR`)
/// a number; empty when none is, or when the sign names more than one (`¥`).
fn currency(before: &str, after: &str) -> String {
    const SIGNS: &[(&str, &str)] = &[
        ("US$", "USD"),
        ("CA$", "CAD"),
        ("AU$", "AUD"),
        ("NZ$", "NZD"),
        ("HK$", "HKD"),
        ("MX$", "MXN"),
        ("C$", "CAD"),
        ("A$", "AUD"),
        ("S$", "SGD"),
        ("R$", "BRL"),
        ("$", "USD"),
        ("€", "EUR"),
        ("£", "GBP"),
        ("₩", "KRW"),
        ("₹", "INR"),
        ("¥", ""),
        ("￥", ""),
    ];
    let (before, after) = (before.to_uppercase(), after.to_uppercase());
    if let Some((_, code)) = SIGNS.iter().find(|(s, _)| before.ends_with(s)) {
        return code.to_string();
    }
    let word = |w: Option<&str>| {
        let w = w
            .unwrap_or_default()
            .trim_matches(|c: char| !c.is_alphanumeric());
        CODES.contains(&w).then(|| w.to_string())
    };
    if let Some(code) = word(before.split_whitespace().next_back()) {
        return code;
    }
    const AFTER: &[(&str, &str)] = &[("원", "KRW"), ("€", "EUR"), ("円", "JPY"), ("元", "CNY")];
    if let Some((_, code)) = AFTER.iter().find(|(s, _)| after.starts_with(s)) {
        return code.to_string();
    }
    word(after.split_whitespace().next()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::sync::mpsc;
    use std::time::Duration;

    /// A stand-in for the real agent-browser: it appends each argv to `argv.log` beside it and
    /// answers `get url`, `get title`, `snapshot` and `eval` with `<what>.json` from its folder
    /// (failing when there is none); anything else prints `done: <argv>`.
    const FAKE: &str = r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\n' "$*" >> "$d/argv.log"
case " $* " in
  *" get url "*) f=url ;;
  *" get title "*) f=title ;;
  *" snapshot "*) f=snapshot ;;
  *" eval "*) f=eval ;;
  *) echo "done: $*"; exit 0 ;;
esac
[ -f "$d/$f.json" ] || { echo '{"success":false,"data":null,"error":"no page"}'; exit 1; }
cat "$d/$f.json"
"#;

    struct T {
        dir: PathBuf,
        env: Env,
    }

    impl Drop for T {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    impl T {
        fn new(name: &str) -> T {
            use std::os::unix::fs::PermissionsExt;
            let dir = crate::test_dir(name);
            let real = dir.join("real-agent-browser");
            std::fs::write(&real, FAKE).unwrap();
            std::fs::set_permissions(&real, std::fs::Permissions::from_mode(0o755)).unwrap();
            let env = Env {
                real,
                socket: Some(dir.join("gate.sock")),
                session: "main".into(),
            };
            T { dir, env }
        }

        fn file(&self, name: &str, data: Value) {
            let text = json!({"success": true, "data": data}).to_string();
            std::fs::write(self.dir.join(name), text).unwrap();
        }

        /// A page at `url` whose snapshot is `lines` (role, name), refs e1, e2, ... in order,
        /// with `extra` text lines after them (a total, say).
        fn page(&self, url: &str, lines: &[(&str, &str)], extra: &str) {
            self.file("url.json", json!({ "url": url }));
            let mut tree = Vec::new();
            let mut refs = serde_json::Map::new();
            for (i, (role, name)) in lines.iter().enumerate() {
                let r = format!("e{}", i + 1);
                tree.push(format!("- {role} {} [ref={r}]", Value::from(*name)));
                refs.insert(r, json!({"role": role, "name": name}));
            }
            let tree = format!("{}\n{extra}", tree.join("\n"));
            let data = json!({"snapshot": tree.trim_end(), "origin": url, "refs": refs});
            self.file("snapshot.json", data);
        }

        /// What the focused element is, as the wrapper's script reports it.
        fn focus(&self, facts: Value) {
            self.file(
                "eval.json",
                json!({"result": facts.to_string(), "origin": "x"}),
            );
        }

        fn argv(&self) -> Vec<String> {
            let log = std::fs::read_to_string(self.dir.join("argv.log")).unwrap_or_default();
            log.lines().map(String::from).collect()
        }

        fn clicked(&self) -> bool {
            self.argv().iter().any(|l| l.starts_with("click"))
        }

        /// A gate on the socket: each request comes out with its connection, for the test to
        /// answer, hold open or drop.
        fn gate(&self) -> mpsc::Receiver<(Value, UnixStream)> {
            let listener = UnixListener::bind(self.env.socket.as_ref().unwrap()).unwrap();
            let (tx, rx) = mpsc::channel();
            std::thread::spawn(move || {
                for conn in listener.incoming() {
                    let Ok(conn) = conn else { return };
                    let mut line = String::new();
                    let mut read = BufReader::new(conn.try_clone().unwrap());
                    if read.read_line(&mut line).is_err() {
                        continue;
                    }
                    let req = serde_json::from_str(&line).unwrap_or(Value::Null);
                    if tx.send((req, conn)).is_err() {
                        return;
                    }
                }
            });
            rx
        }

        fn spawn(&self, args: &[&str]) -> std::thread::JoinHandle<Next> {
            let args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
            let env = self.env.clone();
            std::thread::spawn(move || run(&args, &env))
        }

        fn run(&self, args: &[&str]) -> Next {
            self.spawn(args).join().unwrap()
        }
    }

    fn asked(gate: &mpsc::Receiver<(Value, UnixStream)>) -> (Value, UnixStream) {
        gate.recv_timeout(Duration::from_secs(20))
            .expect("the wrapper never asked the gate")
    }

    fn answer(conn: &mut UnixStream, v: Value) {
        conn.write_all(format!("{v}\n").as_bytes()).unwrap();
    }

    const MAIL: &str = "https://mail.example.com/compose";
    const COMPOSE: &[(&str, &str)] = &[
        ("textbox", "To"),
        ("textbox", "Message"),
        ("button", "Send"),
        ("link", "Inbox"),
    ];

    #[test]
    fn a_navigation_passes_straight_through() {
        let t = T::new("bg-pass");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        for args in [
            &["open", "https://example.com"][..],
            &["snapshot", "-i"],
            &["get", "text", "@e1"],
            &["--version"],
            &["--help"],
            &[],
            &["scroll", "down", "500"],
            &["--session", "work", "back"],
            &["press", "Tab"],
            &["key", "ArrowDown"],
            &["hover", "@e3"],
            &["eval", "document.title"],
            &["batch", "open https://example.com", "snapshot -i"],
            // agent-browser only prints its help or version for these, whatever else is there.
            &["click", "@e3", "--help"],
            &["fill", "@e1", "-h"],
            &["--session", "-V", "press", "Enter"],
        ] {
            assert_eq!(t.run(args), Next::Exec, "{args:?}");
        }
        // Nothing read the page, and the gate heard nothing.
        assert!(t.argv().is_empty(), "{:?}", t.argv());
        assert!(gate.try_recv().is_err());
        // A click on a link reads the page (as the agent's own session), then goes unasked.
        assert_eq!(
            t.run(&["--session", "work", "click", "@e4", "--new-tab"]),
            Next::Exec
        );
        let argv = t.argv();
        assert!(
            argv.contains(&"--session work get url --json".to_string()),
            "{argv:?}"
        );
        assert!(
            argv.iter()
                .any(|l| l.starts_with("--session work snapshot") && l.ends_with("--json")),
            "{argv:?}"
        );
        assert!(!t.clicked(), "the wrapper itself never clicks: {argv:?}");
        assert!(gate.try_recv().is_err());
        // Without a gate at all, what needs none still runs.
        let mut alone = t.env.clone();
        alone.socket = None;
        let args = ["click".to_string(), "@e4".to_string()];
        assert_eq!(run(&args, &alone), Next::Exec);
    }

    #[test]
    fn a_send_click_waits_for_the_verdict() {
        let t = T::new("bg-send");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        let waiting = t.spawn(&["click", "@e3"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["v"], 1, "{req}");
        assert_eq!(req["tool"], "browser");
        assert_eq!(req["session"], "main");
        assert_eq!(req["args"]["argv"], json!(["click", "@e3"]));
        let b = &req["browser"];
        assert_eq!(b["action"], "click", "{req}");
        assert_eq!(b["url"], MAIL);
        assert_eq!(b["origin"], "https://mail.example.com");
        assert_eq!(
            (b["role"].as_str(), b["name"].as_str()),
            (Some("button"), Some("Send"))
        );
        assert_eq!(b["typed"], json!([]));
        // It waits with its side of the socket open: a half-close would read as a hang-up (A7).
        conn.set_read_timeout(Some(Duration::from_millis(300)))
            .unwrap();
        let mut byte = [0u8; 1];
        match conn.read(&mut byte) {
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) => {}
            other => panic!("the wrapper hung up or wrote more: {other:?}"),
        }
        assert!(!waiting.is_finished());
        assert!(!t.clicked(), "clicked before the verdict");
        answer(&mut conn, json!({"verdict": "allow", "id": 1}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        // Denied, or held: refused, in the daemon's words.
        for (verdict, text) in [
            ("deny", "not run: the owner did not approve #2"),
            (
                "hold",
                "held for the owner as #3 (click \"Send\" on https://mail.example.com)",
            ),
            ("deny", "not run: 3 actions already wait for the owner"),
        ] {
            let waiting = t.spawn(&["click", "@e3"]);
            let (_, mut conn) = asked(&gate);
            answer(
                &mut conn,
                json!({"verdict": verdict, "id": 2, "text": text}),
            );
            assert_eq!(waiting.join().unwrap(), Next::Refuse(text.into()));
        }
        // A key press in the composer is a send too: its text goes on the card, its facts checked.
        t.focus(
            json!({"role": "textbox", "name": "Message", "placeholder": "", "editable": true,
                       "value": "hi bob", "textarea": true, "password": false}),
        );
        let waiting = t.spawn(&["press", "Enter"]);
        let (req, mut conn) = asked(&gate);
        let b = &req["browser"];
        assert_eq!(b["action"], "press", "{req}");
        assert_eq!(
            (b["role"].as_str(), b["name"].as_str()),
            (Some("textbox"), Some("Message"))
        );
        assert_eq!(b["composer"], true);
        assert_eq!(b["typed"], json!(["hi bob"]));
        answer(&mut conn, json!({"verdict": "allow", "id": 5}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        assert!(!t.argv().iter().any(|l| l.starts_with("press")));
    }

    #[test]
    fn the_gate_closing_mid_wait_denies() {
        let t = T::new("bg-closed");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        // The daemon restarts while the click waits: its end of the socket closes.
        let waiting = t.spawn(&["click", "@e3"]);
        let (_, conn) = asked(&gate);
        assert!(!waiting.is_finished());
        drop(conn);
        assert_eq!(waiting.join().unwrap(), Next::Refuse(NOT_ANSWERING.into()));
        // An answer the wrapper cannot read is no answer.
        let waiting = t.spawn(&["click", "@e3"]);
        let (_, mut conn) = asked(&gate);
        conn.write_all(b"allow\n").unwrap();
        assert_eq!(waiting.join().unwrap(), Next::Refuse(NOT_ANSWERING.into()));
        // No gate there (the daemon down), or none named: refused, never run.
        let mut gone = t.env.clone();
        gone.socket = Some(t.dir.join("nobody.sock"));
        let args = ["click".to_string(), "@e3".to_string()];
        assert_eq!(run(&args, &gone), Next::Refuse(NOT_ANSWERING.into()));
        gone.socket = None;
        assert_eq!(run(&args, &gone), Next::Refuse(NOT_ANSWERING.into()));
        // Typing is checked for private facts the same way: no gate, no typing.
        let typing = ["fill".to_string(), "@e1".into(), "bob@x.org".into()];
        assert_eq!(run(&typing, &gone), Next::Refuse(NOT_ANSWERING.into()));
        assert!(!t.clicked());
        assert!(!t.argv().iter().any(|l| l.starts_with("fill")));
    }

    #[test]
    fn korean_send_button_is_gated() {
        let t = T::new("bg-korean");
        let gate = t.gate();
        // Review Focus 5's words first, then forms of them.
        for name in [
            "보내기",
            "전송",
            "게시",
            "결제",
            "구매",
            "주문",
            "답장",
            "게시하기",
            "공유",
            "결제하기",
            "바로구매",
            "주문하기",
        ] {
            t.page("https://shop.example.kr/cart", &[("button", name)], "");
            let waiting = t.spawn(&["click", "@e1"]);
            let (req, mut conn) = asked(&gate);
            assert_eq!(req["browser"]["name"], name, "{req}");
            answer(&mut conn, json!({"verdict": "allow", "id": 1}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{name}");
        }
        // A list or a lookup is no action: it goes unasked.
        for name in ["주문 내역", "배송 조회", "검색"] {
            t.page("https://shop.example.kr/cart", &[("button", name)], "");
            assert_eq!(t.run(&["click", "@e1"]), Next::Exec, "{name}");
        }
        assert!(gate.try_recv().is_err());
        // A payment carries the page's labelled total and its title for the owner's card.
        t.page(
            "https://shop.example.kr/cart",
            &[("button", "결제하기")],
            "- StaticText \"상품 금액\"\n- StaticText \"50,000원\"\n- StaticText \"총 결제금액\"\n- StaticText \"54,200원\"",
        );
        t.file("title.json", json!({"title": "장바구니 - 상점"}));
        let waiting = t.spawn(&["click", "@e1"]);
        let (req, mut conn) = asked(&gate);
        let b = &req["browser"];
        assert_eq!(
            b["amount"],
            json!({"value": 54200.0, "currency": "KRW"}),
            "{req}"
        );
        assert_eq!(b["title"], "장바구니 - 상점");
        answer(&mut conn, json!({"verdict": "allow", "id": 1}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
    }

    #[test]
    fn a_control_the_wrapper_cannot_read_is_asked_about() {
        let t = T::new("bg-unread");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        // A ref the page does not have, a snapshot that fails, a focus in another site's frame:
        // the gate is asked with nothing readable, and holds it as a click it could not read.
        let mut cases: Vec<Vec<&str>> = vec![vec!["click", "@e99"], vec!["dblclick", "e99"]];
        cases.push(vec!["find", "text", "Nothing here"]);
        for args in &cases {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            assert_eq!(req["browser"]["role"], "", "{args:?}: {req}");
            answer(
                &mut conn,
                json!({"verdict": "hold", "id": 1, "text": "held"}),
            );
            assert_eq!(waiting.join().unwrap(), Next::Refuse("held".into()));
        }
        // R39: a locator's text the page has nowhere still names the control for its class.
        let waiting = t.spawn(&["find", "text", "Send now", "click"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(
            (
                req["browser"]["role"].as_str(),
                req["browser"]["name"].as_str()
            ),
            (Some(""), Some("Send now"))
        );
        answer(
            &mut conn,
            json!({"verdict": "hold", "id": 1, "text": "held"}),
        );
        waiting.join().unwrap();
        std::fs::remove_file(t.dir.join("snapshot.json")).unwrap();
        let waiting = t.spawn(&["click", "@e3"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["role"], "", "{req}");
        answer(
            &mut conn,
            json!({"verdict": "hold", "id": 1, "text": "held"}),
        );
        waiting.join().unwrap();
        t.focus(json!({"unread": true}));
        let waiting = t.spawn(&["press", "Enter"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["role"], "", "{req}");
        answer(
            &mut conn,
            json!({"verdict": "hold", "id": 1, "text": "held"}),
        );
        waiting.join().unwrap();
        assert!(!t.clicked());
    }

    #[test]
    fn find_and_enter_are_read_from_the_page() {
        let t = T::new("bg-find");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        // `find` matches a part of the name: the page's own control is what counts.
        for args in [
            &["find", "role", "button", "click", "--name", "Sen"][..],
            &["find", "text", "send"],
            &["find", "role", "button"],
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            assert_eq!(req["browser"]["name"], "Send", "{args:?}: {req}");
            assert_eq!(req["browser"]["action"], "click");
            answer(&mut conn, json!({"verdict": "allow", "id": 1}));
            assert_eq!(waiting.join().unwrap(), Next::Exec);
        }
        assert_eq!(t.run(&["find", "text", "Inbox", "click"]), Next::Exec);
        assert_eq!(
            t.run(&["find", "role", "button", "hover", "--name", "Send"]),
            Next::Exec
        );
        assert!(gate.try_recv().is_err());
        // Enter goes unasked in a search box, a box whose placeholder says search (A1), a login
        // form, a link and the page itself.
        for facts in [
            json!({"role": "searchbox", "name": "", "editable": true, "value": "rust"}),
            json!({"role": "textbox", "name": "", "placeholder": "Search mail", "editable": true}),
            json!({"role": "textbox", "name": "Password", "editable": true, "password": true}),
            json!({"role": "link", "name": "Inbox"}),
            json!({"role": "document", "name": ""}),
        ] {
            t.focus(facts.clone());
            assert_eq!(t.run(&["press", "Enter"]), Next::Exec, "{facts}");
        }
        assert!(gate.try_recv().is_err());
        // Typing a line break into a composer presses Enter there: asked about as a key press.
        let waiting = t.spawn(&["type", "@e2", "see you\n"]);
        let (req, mut conn) = asked(&gate);
        let b = &req["browser"];
        assert_eq!(
            (b["action"].as_str(), b["name"].as_str()),
            (Some("press"), Some("Message")),
            "{req}"
        );
        assert_eq!(b["typed"], json!(["see you\n"]));
        answer(&mut conn, json!({"verdict": "allow", "id": 1}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        // A plain `submit` is a post only on a form with a textarea; unread, it is asked about.
        t.page(
            "https://forum.example.com/t/1",
            &[("textbox", "Reply"), ("button", "Submit")],
            "",
        );
        t.file(
            "eval.json",
            json!({"result": json!({"found": 1, "textarea": false}).to_string()}),
        );
        assert_eq!(t.run(&["click", "@e2"]), Next::Exec);
        for eval in [
            Some(json!({"found": 1, "textarea": true})),
            Some(json!({"found": 0})),
            None,
        ] {
            match &eval {
                Some(v) => t.file("eval.json", json!({"result": v.to_string()})),
                None => std::fs::remove_file(t.dir.join("eval.json")).unwrap(),
            }
            let waiting = t.spawn(&["click", "@e2"]);
            let (req, mut conn) = asked(&gate);
            assert_eq!(req["browser"]["form_textarea"], true, "{eval:?}: {req}");
            answer(&mut conn, json!({"verdict": "allow", "id": 1}));
            waiting.join().unwrap();
        }
    }

    #[test]
    fn typing_is_checked_for_private_facts() {
        let t = T::new("bg-typing");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        for (args, action, typed) in [
            (&["fill", "@e1", "bob@x.org"][..], "fill", "bob@x.org"),
            (
                &["type", "@e2", "call", "me", "at", "555"],
                "type",
                "call me at 555",
            ),
            (
                &["type", "@e2", "hi", "--delay", "20", "--clear"],
                "type",
                "hi",
            ),
            (&["keyboard", "type", "my", "number"], "type", "my number"),
            (&["keyboard", "inserttext", "x"], "type", "x"),
            (
                &["find", "label", "Email", "fill", "bob@x.org"],
                "fill",
                "bob@x.org",
            ),
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(b["action"], action, "{args:?}: {req}");
            assert_eq!(b["typed"], json!([typed]), "{args:?}");
            assert_eq!(b["origin"], "https://mail.example.com");
            assert!(!t.argv().iter().any(|l| l.starts_with(args[0])));
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec);
        }
    }

    #[test]
    fn batches_and_the_browsers_own_agents_cannot_carry_a_click() {
        let t = T::new("bg-batch");
        for args in [
            &["batch", "open https://x.org", "click @e3"][..],
            &["batch", r#"["fill", "@e1", "x"]"#],
            &["batch", "--bail", "--session s press Enter"],
            // In a batch, `--help` is no help: the click would run.
            &["batch", "click @e3 --help"],
            &["batch", "\"click\" @e3"],
            &["batch"],
            &["batch", "--bail"],
            &["mcp"],
            &["mcp", "call", "agent_browser_click", "{}"],
            &["chat", "send the mail"],
        ] {
            assert!(matches!(t.run(args), Next::Refuse(_)), "{args:?}");
        }
        assert!(t.argv().is_empty());
    }

    #[test]
    fn the_labelled_total_is_the_largest_one_with_its_currency() {
        let amount = |value: f64, currency: &str| {
            Some(PageAmount {
                value,
                currency: currency.into(),
            })
        };
        for (tree, want) in [
            ("- StaticText \"Order total: $54.20\"", amount(54.2, "USD")),
            ("- cell \"Subtotal\"\n- cell \"$50.00\"\n- cell \"Total\"\n- cell \"$54.20\"", amount(54.2, "USD")),
            ("- StaticText \"Total savings: $5\"\n- StaticText \"Order total\"\n- StaticText \"$1,054.20\"", amount(1054.2, "USD")),
            ("- StaticText \"Amount due\"\n- StaticText \"54,20 €\"", amount(54.2, "EUR")),
            ("- StaticText \"Grand total EUR 1.234,50\"", amount(1234.5, "EUR")),
            ("- StaticText \"총 결제금액\"\n- StaticText \"54,200원\"", amount(54200.0, "KRW")),
            ("- StaticText \"합계 ₩12,000\"", amount(12000.0, "KRW")),
            ("- StaticText \"Total: 3 items\"", amount(3.0, "")),
            ("- StaticText \"Total\"\n- StaticText \"¥5,000\"", amount(5000.0, "")),
            ("- StaticText \"Total\"\n- StaticText \"CA$20\"", amount(20.0, "CAD")),
            ("- StaticText \"Total $10\"\n- StaticText \"Total €20\"", None),
            ("- StaticText \"Subtotal $50\"", None),
            ("- button \"Pay\" [ref=e1]", None),
        ] {
            assert_eq!(page_total(tree), want, "{tree}");
        }
    }

    #[test]
    fn the_wrapper_never_runs_itself() {
        let t = T::new("bg-self");
        let me = std::env::current_exe().unwrap();
        let mut env = t.env.clone();
        env.real = me;
        assert!(env.check_real().is_err());
        env.real = "agent-browser".into();
        assert!(
            env.check_real().is_err(),
            "a bare name is looked up on the PATH: the wrapper"
        );
        assert!(t.env.check_real().is_ok());
    }
}
