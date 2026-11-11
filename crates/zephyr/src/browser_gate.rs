//! The browser wrapper (approvals spec §4.4). `agent-browser` on the agent's PATH is
//! `/opt/zephyr/gate/bin/agent-browser`, a root-owned link to the zephyr binary, which acts as
//! this wrapper when `argv[0]` is `agent-browser` (main.rs, before any config is read: the agent
//! cannot read the daemon's). The real binary lives out of the PATH at
//! `/opt/zephyr/libexec/agent-browser` (`$ZEPHYR_AGENT_BROWSER` names another: tests' fake).
//!
//! Every command goes straight through (`exec`, as it was), except:
//!
//! - a click (`click`, `dblclick`, `tap`, `find … click`) and Enter (`press`/`key`/`keydown`/
//!   `keyup` Enter, with modifiers or none, as 0.41's `parse_key_chord` reads it: N2; `\n` or
//!   `\r`, or a line break typed with `type` or `keyboard type`): the
//!   wrapper asks the real binary for the page's address (`get url`) and for the control, its
//!   role and accessible name (`snapshot`'s refs and tree, scoped to a CSS selector when one
//!   names it; for a key press the focused element, which a read-only script reports), classes
//!   it with `gate::browser_class`, and takes a message, post or payment, or a control it could
//!   not read, to the gate socket. A click lands on what holds its target (a span in a Send
//!   button is the button's: R53); Enter in a form's field also counts the form's default
//!   button, which it submits, and Space or Enter on a focused button (or on an element a click
//!   handler or a tab stop makes one: M3) is a click on it, classed with its form (I3, N1);
//!   anything typed before a line break is checked for the private facts (I1); a line break
//!   typed after a Tab (which moves the focus), or any key typed after the first Enter, lands
//!   where the wrapper did not read, and is held as a control it could not read (N3, R57);
//! - typing (`fill`, `type`, `keyboard type|inserttext`, `find … fill`): its value goes to the
//!   gate first, for the private facts (never gated by class);
//! - `batch` carrying any of these or reading its commands from stdin (each split as 0.41 splits
//!   it: C1), and what acts on the page past the wrapper (agent-browser's `mcp` and `chat`, a
//!   page's own tools through `webmcp invoke`: R55): refused.
//!
//! The wrapper then waits for the gate's verdict with its connection open (A7: a closed one is the
//! action dropped, so a killed wrapper leaves nothing waiting), on a socket the daemon serves
//! (`gate_client` checks its peer: I2). `allow` reads the page again, the control too (a ref's
//! from a full snapshot: M1), and runs the agent's command as it was only if the page is still
//! the one the owner approved (I4); after a wait, it tells the gate on the same connection
//! whether it ran, which is what the owner's record says (M2). Anything else, a
//! closed socket or no gate at all exits 1 with the daemon's words, or [`NOT_ANSWERING`] or
//! [`CHANGED`], on stderr: what the agent reads. Nothing touches the page while it waits, and
//! every scoped snapshot the wrapper takes is followed by a full one, so the agent's refs resolve
//! as before (R54).
//!
//! What is not gated, by design: an unrecognized label (documented limit), `eval`, `mouse`,
//! `select`, `drag` and the rest of the commands (spec §4.4's list), and the real binary run by
//! its path or by a copy (a name check, like the plugin's: spec §2 non-goals).

use std::cell::RefCell;
use std::path::PathBuf;
use std::process::{Command, ExitCode, Stdio};
use std::rc::Rc;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{json, Value};
use zephyr_policy::policy::{self, Class};

use crate::gate::{self, PageAmount};

/// Names the real agent-browser for the wrapper (`gate::agent_env` sets it; tests name a fake).
pub const REAL_ENV: &str = "ZEPHYR_AGENT_BROWSER";
/// Where the image keeps the real agent-browser, out of the agent's PATH.
pub const REAL: &str = "/opt/zephyr/libexec/agent-browser";
/// The root-owned folder whose `agent-browser` is this wrapper, first on the agent's PATH.
pub const GATE_BIN: &str = "/opt/zephyr/gate/bin";
/// Where the image keeps the real xdotool; `xdotool` in [`GATE_BIN`] is this wrapper.
pub const XDOTOOL: &str = "/usr/bin/xdotool";
/// Names another xdotool for the wrapper (tests' fake).
pub const XDOTOOL_ENV: &str = "ZEPHYR_XDOTOOL";
/// Where the daemon's public folder is in the image, for a wrapper told of no gate socket.
const PUBLIC: &str = "/var/lib/zephyr-public";
/// How long a wrapper waits on the gate while the owner holds the desktop: past the gate's own
/// limit (`Config::desktop_hold`, 2 minutes).
const HELD_WAIT: Duration = Duration::from_secs(3 * 60);
/// What the agent reads when the gate cannot be asked or does not answer.
pub const NOT_ANSWERING: &str = "the gate is not answering; not run";
/// What the agent reads when the control or the page is not what the owner approved (I4).
pub const CHANGED: &str = "the page changed while the owner decided; not run";

const BATCH: &str = "not run: agent-browser batch takes no clicks, key presses or typing here, \
                     which the approvals gate checks one by one; run each as a command of its own";
const BATCH_STDIN: &str = "not run: give agent-browser batch its commands as arguments here, so \
                           the approvals gate can check them";
const AGENTS: &str = "not run: agent-browser's mcp, chat and webmcp invoke act on the page past \
                      the approvals gate; use its commands";

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
    finish("agent-browser", run(&args, &env), &env, &args)
}

/// Runs the real binary in this process's place, or refuses with exit 1.
fn finish(name: &str, next: Next, env: &Env, args: &[String]) -> ExitCode {
    match next {
        Next::Exec => {
            use std::os::unix::process::CommandExt;
            let e = Command::new(&env.real).args(args).exec();
            eprintln!("{name}: running {}: {e}", env.real.display());
            ExitCode::from(127)
        }
        Next::Refuse(text) => {
            eprintln!("{text}");
            ExitCode::FAILURE
        }
    }
}

/// `xdotool ARGS` as the agent runs it (DESIGN §4.11): the real one in this process's place,
/// unless the owner holds the desktop. Held whatever its arguments.
pub fn xdotool_main(args: Vec<String>) -> ExitCode {
    let mut env = Env::from_process();
    let named = std::env::var_os(XDOTOOL_ENV).filter(|v| !v.is_empty());
    env.real = named.map_or_else(|| PathBuf::from(XDOTOOL), PathBuf::from);
    // A bare name would be looked up on the PATH, where `xdotool` is this wrapper.
    let me = std::fs::canonicalize("/proc/self/exe").ok();
    if !env.real.is_absolute() || std::fs::canonicalize(&env.real).ok() == me {
        eprintln!("xdotool: {} is not the real xdotool", env.real.display());
        return ExitCode::from(127);
    }
    finish("xdotool", xdotool(&env), &env, &args)
}

/// What the `xdotool` wrapper does with any command.
pub fn xdotool(env: &Env) -> Next {
    held(env).map_or(Next::Exec, Next::Refuse)
}

/// Whether the owner holds the desktop (DESIGN §4.11): `Some(text)` means do not run, and exit 1
/// with it. Root's `desktop-held`, beside the gate socket, says so; the wrapper then waits on
/// the gate, which answers at the hand back or after 2 minutes with the words to print, or with
/// none when the desktop is not held after all. Held with no gate answering is refused.
pub fn held(env: &Env) -> Option<String> {
    let dir = env.socket.as_deref().and_then(std::path::Path::parent);
    let flag = dir
        .unwrap_or(std::path::Path::new(PUBLIC))
        .join(crate::desktop::HELD_FILE);
    // Only a file that is not there is no hold: one that cannot be read is asked about.
    if matches!(std::fs::symlink_metadata(flag), Err(e) if e.kind() == std::io::ErrorKind::NotFound)
    {
        return None;
    }
    let socket = env.socket.as_ref();
    let req = json!({"v": 1, "desktop": "wait", "session": env.session});
    match socket.map(|s| zephyr_policy::gate_client::ask_open(s, &req, HELD_WAIT)) {
        Some(Ok((a, _))) if a["verdict"] == "deny" && a["text"].is_string() => {
            let text = a["text"].as_str().unwrap_or_default();
            (!text.is_empty()).then(|| text.to_string())
        }
        _ => Some(NOT_ANSWERING.into()),
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
    // Before the page is read: the owner has the desktop, and nothing of the agent's touches it.
    if let Some(text) = held(env) {
        return Next::Refuse(text);
    }
    let argv = split(args);
    let flags = forwarded(&argv.globals);
    let page = Page::new(env, &flags);
    let act = what(&argv.rest);
    match &act {
        Act::Pass => return Next::Exec,
        Act::Refuse(text) => return Next::Refuse((*text).into()),
        Act::Type { action, typed } => {
            let seen = Seen {
                typed: vec![typed.clone()],
                ..Seen::default()
            };
            let (next, wait) = ask_gate(env, args, &request(action, &page.url(), &seen));
            if next == Next::Exec && !said(wait, true) {
                return Next::Refuse(NOT_ANSWERING.into());
            }
            return next;
        }
        Act::Click(_) | Act::Key { .. } => {}
    }
    let url = page.url();
    let Some(Look { action, mut seen }) = look(&page, &act) else {
        return Next::Exec;
    };
    let class = gate::browser_class(&request(action, &url, &seen));
    // Through at once: a control read and no message, post or payment, with nothing typed that
    // could hold a private fact (I1: a line break typed anywhere sends what came before it).
    let typed = seen.typed.iter().any(|t| !t.trim().is_empty());
    if class.is_none() && !seen.role.trim().is_empty() && !typed {
        return Next::Exec;
    }
    if class == Some(Class::Money) {
        seen.amount = page.total();
        seen.title = page.title();
    }
    let first = Look { action, seen };
    let wait = match ask_gate(env, args, &request(action, &url, &first.seen)) {
        (Next::Exec, wait) => wait,
        (refused, _) => return refused,
    };
    // I4: up to 10 minutes went by; what the owner approved must still be what runs.
    let same = unchanged(&Page::new(env, &flags), &act, &url, &first, class);
    match (same, said(wait, same)) {
        (true, true) => Next::Exec,
        (true, false) => Next::Refuse(NOT_ANSWERING.into()),
        (false, _) => Next::Refuse(CHANGED.into()),
    }
}

/// M2: an action the owner let go (an `allow` after a wait, with its number) is recorded by the
/// daemon only as the wrapper says, on the connection the `allow` came on: `{"ran": true}` just
/// before the command runs, `{"ran": false}` when it will not. Returns whether it was said (or
/// there was nothing to say): what the gate cannot be told ran is not run.
fn said(wait: Option<std::os::unix::net::UnixStream>, ran: bool) -> bool {
    use std::io::Write;
    let Some(mut conn) = wait else {
        return true;
    };
    let line = format!("{}\n", json!({ "ran": ran }));
    conn.set_write_timeout(Some(FACT_WAIT)).is_ok() && conn.write_all(line.as_bytes()).is_ok()
}

/// What the wrapper read for a click or a key press: the action the gate is told of (`click` or
/// `press`) and the control.
struct Look {
    action: &'static str,
    seen: Seen,
}

/// The control `act` lands on, read from the page; `None` for an action that does nothing the
/// gate looks at (Space anywhere but on a button). Space or Enter on a focused button is a click
/// on it (I3); Enter in a form's field reads what the form submits.
fn look(page: &Page, act: &Act) -> Option<Look> {
    match act {
        Act::Click(target) => Some(Look {
            action: "click",
            seen: page.submit_form(page.control(target)),
        }),
        Act::Key {
            blind: true, typed, ..
        } => {
            // Where these keys land is not where the wrapper could read: a control it could
            // not read, which the gate holds, with what is typed for the facts check.
            let seen = Seen {
                typed: typed.clone(),
                ..Seen::default()
            };
            Some(Look {
                action: "press",
                seen,
            })
        }
        Act::Key {
            target, typed, key, ..
        } => {
            let mut seen = match target {
                Target::Focus => page.focused(),
                target => {
                    let mut seen = page.control(target);
                    seen.composer = matches!(seen.role.as_str(), "textbox" | "searchbox");
                    seen
                }
            };
            // B7: 0.41 focuses a target without checking that it took the focus, so a key typed
            // into one that takes no text (and is no button a key presses) lands wherever the
            // focus was: where the wrapper did not read, held as unread (R57).
            let lands = |role: &str| editable(role) || pressable(role) || role.trim().is_empty();
            if !matches!(target, Target::Focus) && !lands(&seen.role) {
                let seen = Seen {
                    typed: typed.clone(),
                    ..Seen::default()
                };
                return Some(Look {
                    action: "press",
                    seen,
                });
            }
            if pressable(&seen.role) {
                // A button pressed by a key: what clicking it does, classed as a click on it is
                // (N1): its form's textarea kept and a plain `submit` looked up the same way, but
                // never its form's password field, which a click never reads either (round 3:
                // a checkout's optional account password must not make its Pay pass); nothing
                // typed into it.
                let pressed = Seen {
                    role: seen.role,
                    name: seen.name,
                    textarea: seen.textarea,
                    ..Seen::default()
                };
                return Some(Look {
                    action: "click",
                    seen: page.submit_form(pressed),
                });
            }
            let unread = seen.role.trim().is_empty();
            if *key == Key::Space && !unread {
                return None; // a space typed, or the page scrolled
            }
            if !unread && !matches!(target, Target::Focus) {
                seen = page.with_form(target, seen);
            }
            seen.typed.extend(typed.iter().cloned());
            Some(Look {
                action: "press",
                seen,
            })
        }
        Act::Pass | Act::Refuse(_) | Act::Type { .. } => None,
    }
}

/// Whether the page, read again after the owner's `allow`, still shows what the owner approved
/// (I4): the same site, the same control (its role, name, form and what is typed), and for a
/// payment, the same total. A ref's control is read again too, from one full snapshot (M1): a
/// ref whose node went away falls back to another with its role and name (0.41 element.rs), and
/// a node kept may have been relabelled.
fn unchanged(page: &Page, act: &Act, url: &str, first: &Look, class: Option<Class>) -> bool {
    if origin_of(&page.url()) != origin_of(url) {
        return false;
    }
    match look(page, act) {
        Some(again) if again.action == first.action && again.seen.same(&first.seen) => {}
        _ => return false,
    }
    class != Some(Class::Money) || page.total() == first.seen.amount
}

/// A role a key press activates as a click: Space or Enter on a focused button.
fn pressable(role: &str) -> bool {
    matches!(role, "button" | "menuitem")
}

/// A role that takes typed text, and so the focus `type` gives it (B7).
fn editable(role: &str) -> bool {
    matches!(
        role.trim().to_lowercase().as_str(),
        "textbox" | "searchbox" | "combobox" | "contenteditable"
    )
}

/// The gate's verdict on the request (spec §4.2): `allow` runs the command; `hold` and `deny`
/// refuse it with the daemon's text; no gate, a closed socket or an answer that cannot be read
/// refuse it with [`NOT_ANSWERING`]. The connection stays open until the answer (A7), and after
/// an `allow` that ends a wait (it names the action), for [`said`].
fn ask_gate(
    env: &Env,
    args: &[String],
    browser: &gate::Browser,
) -> (Next, Option<std::os::unix::net::UnixStream>) {
    let Some(socket) = &env.socket else {
        return (Next::Refuse(NOT_ANSWERING.into()), None);
    };
    let req = json!({
        "v": 1,
        "session": env.session,
        "tool": "browser",
        "args": { "argv": args },
        "browser": browser,
    });
    match zephyr_policy::gate_client::ask_open(socket, &req, VERDICT_WAIT) {
        Ok((a, conn)) if a["verdict"] == "allow" => (Next::Exec, a["id"].is_i64().then_some(conn)),
        Ok((a, _)) if matches!(a["verdict"].as_str(), Some("hold" | "deny")) => {
            let text = a["text"].as_str().filter(|t| !t.trim().is_empty());
            let text = text.unwrap_or("not run: the gate did not allow it");
            (Next::Refuse(text.into()), None)
        }
        _ => (Next::Refuse(NOT_ANSWERING.into()), None),
    }
}

/// The request's `browser` part, which the gate classes as the wrapper does
/// (`gate::browser_class`).
fn request(action: &str, url: &str, seen: &Seen) -> gate::Browser {
    gate::Browser {
        action: action.into(),
        url: url.into(),
        origin: origin_of(url),
        role: seen.role.clone(),
        name: seen.name.clone(),
        typed: seen.typed.clone(),
        form_textarea: seen.textarea,
        form_password: seen.password,
        composer: seen.composer,
        amount: seen.amount.clone(),
        title: seen.title.clone(),
        submit: (seen.submit.as_ref()).map(|c| gate::Element {
            role: c.role.clone(),
            name: c.name.clone(),
        }),
    }
}

/// `https://mail.example.com` for a page there; empty when its address cannot be read.
fn origin_of(url: &str) -> String {
    crate::vault::origin(url).map_or_else(String::new, |o| o.ascii_serialization())
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
    /// Enter or Space in the focused element, or Enter in the target a line break is typed into.
    /// `blind`: typed keys that land where the wrapper cannot read (R57): a line break after a
    /// Tab, which moves the focus, or any key after the first Enter, which may have sent and
    /// moved it.
    Key {
        target: Target,
        typed: Vec<String>,
        key: Key,
        blind: bool,
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
        // A key going up can send too (a page's own handler); keydown and keyup send the key
        // as named (0.41), press reads `\n` and chords as Enter.
        "press" | "key" | "keydown" | "keyup" => match args.first().and_then(|k| key_of(k)) {
            Some(key) => Act::Key {
                target: Target::Focus,
                typed: Vec::new(),
                key,
                blind: false,
            },
            None => Act::Pass,
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
        // R55: `webmcp invoke` calls a page's own tool (a send, a checkout); listing and reading
        // them do nothing.
        "webmcp" => match args.first().map(String::as_str) {
            Some("invoke") => Act::Refuse(AGENTS),
            _ => Act::Pass,
        },
        _ => Act::Pass,
    }
}

/// Typing `typed`: with a line break where Enter would go (`enter_in`), a key press there, which
/// 0.41 sends as a real Enter for each `\n` or `\r` (`type_text_into_active_context`), however
/// little else is typed (C2); else a facts check. Nothing typed is nothing to check. 0.41 sends
/// each `\t` as a real Tab too, which moves the focus, and the rest as text (`insertText`): a
/// line break after a Tab, or a Tab or line break after the first one, is `blind` (N3, R57).
fn typing(action: &'static str, typed: String, enter_in: Option<Target>) -> Act {
    match enter_in {
        Some(target) if typed.contains(['\n', '\r']) => {
            let first = typed.find(['\n', '\r']).unwrap_or_default();
            let (before, after) = (&typed[..first], &typed[first + 1..]);
            Act::Key {
                target,
                blind: before.contains('\t') || after.contains(['\n', '\r', '\t']),
                typed: vec![typed],
                key: Key::Enter,
            }
        }
        _ if typed.trim().is_empty() => Act::Pass,
        _ => Act::Type { action, typed },
    }
}

/// A key the wrapper looks at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Key {
    /// Sends: a message in a composer, a form through its default button, a focused button.
    Enter,
    /// Presses a focused button.
    Space,
}

/// agent-browser 0.41's `parse_key_chord` (cli/src/native/actions.rs), ported as it is: how
/// `press` reads `Control+Shift+Enter`. The parts between `+` that name a modifier (`alt`,
/// `control`/`ctrl`, `meta`/`cmd`/`command`, `shift`, any case, wherever they stand) are taken
/// out, and the rest, joined by `+` again, is the key, with the modifiers' CDP bits (1 Alt,
/// 2 Control, 4 Meta, 8 Shift). With no modifier, the whole text is the key (`+`, `a+Enter`).
pub fn parse_key_chord(input: &str) -> (String, Option<i32>) {
    let parts: Vec<&str> = input.split('+').collect();
    if parts.len() < 2 {
        return (input.to_string(), None);
    }
    let mut modifiers = 0i32;
    let mut key_parts: Vec<&str> = Vec::new();
    for part in &parts {
        match part.to_lowercase().as_str() {
            "alt" => modifiers |= 1,
            "control" | "ctrl" => modifiers |= 2,
            "meta" | "cmd" | "command" => modifiers |= 4,
            "shift" => modifiers |= 8,
            _ => key_parts.push(part),
        }
    }
    if modifiers == 0 {
        return (input.to_string(), None);
    }
    let actual_key = match key_parts.is_empty() {
        true => input.to_string(),
        false => key_parts.join("+"),
    };
    (actual_key, Some(modifiers))
}

/// The key a `press` sends, as 0.41 reads it (N2): [`parse_key_chord`], then `named_key_info`
/// (native/interaction.rs), modifiers or none. `Enter` or `Return` in any case, or a single
/// `\n` or `\r`, is Enter; `Space` or a single space is Space. `NumpadEnter`, which 0.41 sends as
/// a key of that name and code, counts as Enter too: a page's own handler may read its code.
/// `keydown`/`keyup` send their key as named, unparsed, so reading them this way finds every
/// Enter they send.
fn key_of(key: &str) -> Option<Key> {
    let (key, _) = parse_key_chord(key);
    match key.to_lowercase().as_str() {
        "enter" | "return" | "numpadenter" | "\n" | "\r" => Some(Key::Enter),
        "space" | " " => Some(Key::Space),
        _ => None,
    }
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

/// A batch passes only when none of its commands would need the gate; one read from stdin cannot
/// be seen first, so it is refused. Each command is split as 0.41 splits it
/// ([`shell_words_split`], never read as JSON) and parsed as it stands, no flag taken out
/// (`run_batch` calls `parse_command` on it); it is also read the top level's way, flags out,
/// and either reading that needs the gate refuses the batch (C1).
fn batch(args: &[String]) -> Act {
    let commands: Vec<&String> = (args.iter())
        .filter(|a| !matches!(a.as_str(), "--bail" | "--omit-command"))
        .collect();
    if commands.is_empty() {
        return Act::Refuse(BATCH_STDIN);
    }
    for c in commands {
        let words = shell_words_split(c);
        if words.is_empty() {
            continue; // 0.41 skips an empty command
        }
        if what(&words) != Act::Pass || what(&split(&words).rest) != Act::Pass {
            return Act::Refuse(BATCH);
        }
    }
    Act::Pass
}

// ---- the page, read through the real binary ----

/// What the wrapper read of the control: what `gate::Browser` carries. No `Debug`: what is
/// typed can hold a private fact.
#[derive(Clone, Default)]
struct Seen {
    role: String,
    name: String,
    textarea: bool,
    password: bool,
    composer: bool,
    /// For a key press in a form's field: the form's default button (I3).
    submit: Option<Control>,
    typed: Vec<String>,
    amount: Option<PageAmount>,
    title: String,
}

impl Seen {
    /// The same control, read twice (I4): everything its class comes from, and what is typed.
    fn same(&self, other: &Seen) -> bool {
        (self.role == other.role && self.name == other.name)
            && (self.textarea, self.password, self.composer)
                == (other.textarea, other.password, other.composer)
            && self.submit == other.submit
            && self.typed == other.typed
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

/// A control as a snapshot (or a page script) names it. An empty role is a control the wrapper
/// could not read, which the gate holds (R39); its name still gives its class.
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

    /// `{role, name}` from a page script; `None` for anything else.
    fn of(v: &Value) -> Option<Control> {
        let s = |k: &str| v[k].as_str().map(|s| s.trim().to_string());
        Some(Control {
            role: s("role")?,
            name: s("name").unwrap_or_default(),
        })
    }

    fn unread(name: String) -> Control {
        Control {
            role: String::new(),
            name,
        }
    }
}

/// The roles agent-browser 0.41 counts as interactive (snapshot.rs `INTERACTIVE_ROLES`, but an
/// iframe, whose click lands on whatever is inside it): what a click on one lands on is itself.
const INTERACTIVE: &[&str] = &[
    "button",
    "link",
    "textbox",
    "checkbox",
    "radio",
    "combobox",
    "listbox",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "searchbox",
    "slider",
    "spinbutton",
    "switch",
    "tab",
    "treeitem",
];

/// A snapshot: its refs (each control's role and name), its text and that text's lines.
struct Snap {
    refs: Vec<(String, Control)>,
    nodes: Vec<Line>,
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
        let nodes = tree.lines().filter_map(line).collect();
        Some(Snap { refs, nodes, tree })
    }

    /// The lines at the top of the tree: what a snapshot scoped to a selector matched.
    fn top(&self) -> Vec<usize> {
        let depth = self.nodes.iter().map(|l| l.depth).min();
        (0..self.nodes.len())
            .filter(|&i| Some(self.nodes[i].depth) == depth)
            .collect()
    }

    /// The control a click on ref `r` lands on ([`Snap::receiver`]): one nothing clickable
    /// holds is one the wrapper could not read, named by its text (R53). `None`: no such ref.
    fn by_ref(&self, r: &str) -> Option<Control> {
        let (_, control) = self.refs.iter().find(|(id, _)| id == r)?;
        let at = (self.nodes.iter()).position(|n| n.reference.as_deref() == Some(r));
        Some(match at {
            Some(i) => (self.receiver(i)).unwrap_or_else(|| Control::unread(self.text_of(i))),
            None if INTERACTIVE.contains(&control.role.as_str()) => control.clone(),
            None => Control::unread(control.name.clone()),
        })
    }

    /// What a click on line `i` lands on (R53): the line itself when it is interactive, else the
    /// nearest line holding it that is; one agent-browser marks clickable or focusable (a `div`
    /// with a click handler, no role) is a button named by its text. `None` when nothing in the
    /// tree is.
    fn receiver(&self, i: usize) -> Option<Control> {
        let mut depth = self.nodes[i].depth;
        if let Some(c) = self.as_control(i) {
            return Some(c);
        }
        for j in (0..i).rev() {
            if self.nodes[j].depth >= depth {
                continue;
            }
            depth = self.nodes[j].depth;
            if let Some(c) = self.as_control(j) {
                return Some(c);
            }
        }
        None
    }

    /// Line `i` as a control a click lands on: an interactive role with the browser's own name
    /// for it, or one agent-browser's scan found clickable (a button named by its text) or
    /// editable; `None` for any other.
    fn as_control(&self, i: usize) -> Option<Control> {
        let n = &self.nodes[i];
        let (role, name) = match n.cursor.as_deref() {
            _ if INTERACTIVE.contains(&n.role.as_str()) => (n.role.clone(), n.name.clone()),
            Some("clickable" | "focusable") => ("button".into(), self.text_of(i)),
            Some("editable") => ("textbox".into(), n.name.clone()),
            _ => return None,
        };
        Some(Control { role, name })
    }

    /// Line `i`'s name, or with none the names of the lines it holds, as a button's text.
    fn text_of(&self, i: usize) -> String {
        let n = &self.nodes[i];
        if !n.name.trim().is_empty() {
            return n.name.trim().to_string();
        }
        let inside = (self.nodes[i + 1..].iter()).take_while(|l| l.depth > n.depth);
        let words: Vec<&str> = (inside.map(|l| l.name.trim()))
            .filter(|t| !t.is_empty())
            .collect();
        words.join(" ").chars().take(200).collect()
    }
}

/// The real binary, asked as the agent's own command would be (its session, its browser).
struct Page<'a> {
    env: &'a Env,
    flags: &'a [String],
    /// The page's full snapshot, taken once (and again after a scoped one: [`Page::restore`]).
    snap: RefCell<Option<Rc<Option<Snap>>>>,
}

impl<'a> Page<'a> {
    fn new(env: &'a Env, flags: &'a [String]) -> Page<'a> {
        Page {
            env,
            flags,
            snap: RefCell::new(None),
        }
    }

    /// `agent-browser FLAGS ARGS --json`: the answer's `data`, `None` when it fails, says it did
    /// not succeed, or takes longer than [`FACT_WAIT`].
    fn ask(&self, args: &[&str]) -> Option<Value> {
        use std::io::Read;
        let mut child = Command::new(&self.env.real)
            .args(self.flags)
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

    /// The page's labelled total, from its snapshot.
    fn total(&self) -> Option<PageAmount> {
        self.with_snap(|s| page_total(&s.tree)).flatten()
    }

    /// `f` of the page's full snapshot, taken once; `None` when it cannot be taken. A snapshot
    /// gives each element the ref it had (0.41 `durable_ref`), so the agent's refs keep theirs.
    fn with_snap<R>(&self, f: impl FnOnce(&Snap) -> R) -> Option<R> {
        let taken = self.snap.borrow().clone();
        let snap = taken.unwrap_or_else(|| {
            let snap = Rc::new(self.ask(&["snapshot"]).as_ref().and_then(Snap::of));
            *self.snap.borrow_mut() = Some(snap.clone());
            snap
        });
        Option::as_ref(&snap).map(f)
    }

    /// R54: a snapshot rebuilds agent-browser's ref map from what it read (0.41 `handle_snapshot`
    /// calls `begin_snapshot`, which clears it). A scoped one re-adds the refs of the page's own
    /// document but not a frame's, and none at all when its selector matches nothing there; so
    /// after each, a full one is taken, which leaves the map as the agent's own snapshot did.
    fn restore(&self) {
        *self.snap.borrow_mut() = None;
        self.with_snap(|_| ());
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

    /// The control `target` names, as a click on it lands (R53). Nothing readable (no such
    /// ref, a failed snapshot) is a control with neither role nor name, which the gate holds; a
    /// locator whose text the page has nowhere keeps that text as the name, for its class (R39).
    fn control(&self, target: &Target) -> Seen {
        match target {
            Target::Focus => self.focused(),
            Target::Selector(sel) => {
                if let Some(r) = ref_of(sel) {
                    let found = self.with_snap(|s| s.by_ref(r)).flatten();
                    return found.map_or_else(Seen::default, |c| c.seen());
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
                    let fits = |c: &Control| {
                        c.role.eq_ignore_ascii_case(value)
                            && name
                                .as_deref()
                                .is_none_or(|n| matches_text(&c.name, n, *exact))
                    };
                    let all = self.with_snap(|s| {
                        let ids = s.refs.iter().filter(|(_, c)| fits(c));
                        ids.filter_map(|(id, _)| s.by_ref(id)).collect()
                    });
                    strictest(all.unwrap_or_default()).map_or_else(Seen::default, |c| c.seen())
                }
                "text" | "label" | "placeholder" | "alt" | "title" => self.named(value, *exact),
                "testid" => self.scoped(&format!("[data-testid={}]", Value::from(value.as_str()))),
                _ => self.scoped(value), // first, last, nth: a CSS selector
            },
        }
    }

    /// The strictest control a click on an element whose name holds `text` (all of it with
    /// `exact`) lands on; none is `text` itself, with no role.
    fn named(&self, text: &str, exact: bool) -> Seen {
        let found = self.with_snap(|s| {
            let ids = s
                .refs
                .iter()
                .filter(|(_, c)| matches_text(&c.name, text, exact));
            ids.filter_map(|(id, _)| s.by_ref(id)).collect::<Vec<_>>()
        });
        let Some(found) = found else {
            return Seen::default();
        };
        match strictest(found) {
            Some(c) => c.seen(),
            None => Seen {
                name: text.to_string(),
                ..Seen::default()
            },
        }
    }

    /// The strictest control a CSS selector's match lands on, read from a snapshot scoped to
    /// it. A match with no interactive role is classed by what holds it in the page, which a
    /// scoped snapshot does not show (R53): a script finds its nearest clickable ancestor.
    fn scoped(&self, selector: &str) -> Seen {
        let scoped = self.ask(&["snapshot", "-s", selector]);
        self.restore();
        let Some(snap) = scoped.as_ref().and_then(Snap::of) else {
            return Seen::default();
        };
        let (mut found, mut loose) = (Vec::new(), None);
        for i in snap.top() {
            match snap.receiver(i) {
                Some(c) => found.push(c),
                None => {
                    loose.get_or_insert(i);
                }
            }
        }
        if let Some(i) = loose {
            let setup = format!("const sel = {};", Value::from(selector));
            let holder = self.eval(&script("holder", &setup, HOLDER_BODY));
            let holder = holder.filter(|v| v["unread"] != true);
            found.push(holder.as_ref().and_then(Control::of).unwrap_or_else(|| {
                // Nothing read: a control the gate could not read, named by its text.
                Control::unread(snap.text_of(i))
            }));
        }
        strictest(found).map_or_else(Seen::default, |c| c.seen())
    }

    /// `seen` (a button clicked or pressed) with its form read when it is a plain `submit`, a
    /// post on a form with a textarea: what was read of its form already (the focus) counts, else
    /// a script looks, and a form it cannot read counts as one with a textarea.
    fn submit_form(&self, mut seen: Seen) -> Seen {
        if plain_submit(&seen) && !seen.textarea {
            seen.textarea = match self.eval(&script("submit", "", SUBMIT_BODY)) {
                Some(v) if v["found"].as_u64().is_some_and(|n| n > 0) => v["textarea"] == true,
                _ => true,
            };
        }
        seen
    }

    /// `seen` (a field the agent types a line break into) with what Enter submits there (I3):
    /// the form a script finds it in by its selector (or, for a ref or a text, by its name: a
    /// script cannot reach an element by ref), with that form's flags and default button; of
    /// several forms, the strictest. Not read: a control the gate could not read.
    fn with_form(&self, target: &Target, seen: Seen) -> Seen {
        let css = match target {
            Target::Selector(s) if ref_of(s).is_none() && !s.starts_with("text=") => Some(s),
            _ => None,
        };
        let setup = format!(
            "const sel = {}, name = {};",
            css.map_or(Value::Null, |s| Value::from(s.as_str())),
            Value::from(seen.name.as_str()),
        );
        let answer = self.eval(&script("forms", &setup, FORMS_BODY));
        let forms = answer.as_ref().and_then(|v| v["forms"].as_array());
        let Some(forms) = forms else {
            return Seen {
                role: String::new(),
                ..seen
            };
        };
        let with = |f: &Value| Seen {
            textarea: f["textarea"] == true,
            password: f["password"] == true,
            submit: Control::of(&f["submit"]),
            ..seen.clone()
        };
        let class = |s: &Seen| gate::browser_class(&request("press", "", s));
        let ranked = forms.iter().map(with).enumerate();
        let best = ranked.max_by_key(|(i, s)| (class(s), std::cmp::Reverse(*i)));
        best.map_or(seen, |(_, s)| s)
    }

    /// The focused element, read by a script (a snapshot does not mark focus): its role and name
    /// with its placeholder folded in (a search box named only by its placeholder stays one), its
    /// text (what Enter sends, checked for facts and shown on the card), its form and that form's
    /// default button (what Enter in a field submits: I3).
    fn focused(&self) -> Seen {
        let Some(v) = self.eval(&script("focus", "", FOCUS_BODY)) else {
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
            submit: Control::of(&v["submit"]),
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
/// message), one the wrapper could not read before one it read, else the first.
fn strictest(controls: Vec<Control>) -> Option<Control> {
    let rank = |c: &Control| {
        let class = policy::browser_class(&c.role, &c.name, false, false, false);
        (class, c.role.trim().is_empty())
    };
    let ranked = controls.into_iter().enumerate();
    let best = ranked.max_by_key(|(i, c)| (rank(c), std::cmp::Reverse(*i)));
    best.map(|(_, c)| c)
}

/// A page script: `/*zephyr:KIND*/` first (tests' fake browser answers by it), then
/// [`JS_LIB`], `setup` (the script's inputs, as JSON) and `body`. Each reads the page only and
/// returns JSON text.
fn script(kind: &str, setup: &str, body: &str) -> String {
    format!("/*zephyr:{kind}*/(() => {{\n{JS_LIB}\n{setup}\n{body}\n}})()")
}

/// What the page scripts share: an element's role and accessible name as the browser would give
/// them (near enough: the snapshot's are the browser's own), and a form's facts.
const JS_LIB: &str = r#"const INPUTS = { search: 'searchbox', button: 'button', submit: 'button', reset: 'button',
  image: 'button', checkbox: 'checkbox', radio: 'radio', range: 'slider', number: 'spinbutton' };
const INTERACTIVE = { button: 1, link: 1, textbox: 1, checkbox: 1, radio: 1, combobox: 1,
  listbox: 1, menuitem: 1, menuitemcheckbox: 1, menuitemradio: 1, option: 1, searchbox: 1,
  slider: 1, spinbutton: 1, switch: 1, tab: 1, treeitem: 1 };
// The roles named by their content (a field's content is what is typed in it).
const BY_CONTENT = { button: 1, link: 1, menuitem: 1, menuitemcheckbox: 1, menuitemradio: 1,
  option: 1, tab: 1, treeitem: 1 };
const raw = n => ((n && (n.innerText || n.textContent)) || '').trim();
const text = n => raw(n).replace(/\s+/g, ' ');
const roleOf = e => {
  const explicit = (e.getAttribute('role') || '').trim().split(/\s+/)[0].toLowerCase();
  if (explicit) return explicit;
  const tag = e.tagName.toLowerCase(), type = (e.getAttribute('type') || '').toLowerCase();
  if (tag === 'textarea') return 'textbox';
  if (tag === 'input') return INPUTS[type] || 'textbox';
  if (tag === 'button' || tag === 'summary') return 'button';
  if ((tag === 'a' || tag === 'area') && e.hasAttribute('href')) return 'link';
  if (tag === 'select') return 'combobox';
  if (e.isContentEditable) return 'textbox';
  return 'generic';
};
const nameOf = (e, byContent) => {
  const doc = e.ownerDocument;
  const by = (e.getAttribute('aria-labelledby') || '').split(/\s+/).filter(Boolean)
    .map(id => text(doc.getElementById(id))).join(' ').trim();
  const labels = e.labels ? Array.from(e.labels).map(text).join(' ').trim() : '';
  let name = (e.getAttribute('aria-label') || '').trim() || by || labels;
  if (!name && byContent) name = text(e) || e.value || '';
  if (!name) name = e.getAttribute('title') || e.getAttribute('alt') || '';
  return String(name).replace(/\s+/g, ' ').trim();
};
const find = sel => {
  try {
    if (sel.startsWith('xpath=')) return document.evaluate(sel.slice(6), document, null,
      XPathResult.FIRST_ORDERED_NODE_TYPE, null).singleNodeValue;
    return document.querySelector(sel);
  } catch (_) { return null; }
};
const up = n => n.parentElement || (n.parentNode && n.parentNode.host) || null;
const pointer = n => { try { return getComputedStyle(n).cursor === 'pointer'; } catch (_) { return false; } };
// What agent-browser's own scan finds clickable with no interactive role (R53): a click handler,
// a tab stop, or a pointer cursor of its own (not one it takes from its parent).
const clickable = n => {
  const tab = n.getAttribute('tabindex'), parent = up(n);
  const own = pointer(n) && !(parent && parent.nodeType === 1 && pointer(parent));
  return n.hasAttribute('onclick') || n.onclick !== null || (tab !== null && tab !== '-1') || own;
};
const formOf = e => e.form || (e.closest && e.closest('form')) || null;
const submitOf = form => {
  if (!form) return null;
  const all = form.ownerDocument.querySelectorAll('button, input[type="submit" i], input[type="image" i]');
  for (const b of all) {
    if (b.form === form && (b.type === 'submit' || b.type === 'image')) {
      return { role: 'button', name: nameOf(b, true) };
    }
  }
  return null;
};
const formFacts = form => ({
  textarea: !!(form && form.querySelector('textarea, [contenteditable]:not([contenteditable="false"])')),
  password: !!(form && form.querySelector('input[type="password" i]')),
  submit: submitOf(form),
});"#;

/// The focused element's facts: `{unread}` inside another site's frame, `{role: "document"}`
/// when nothing is focused; for a field in a form, the form's facts and default button. An
/// element with no interactive role that is clickable as R53 counts one is a button (M3).
const FOCUS_BODY: &str = r#"let e = document.activeElement;
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
const field = tag === 'textarea' || (tag === 'input' && (!(type in INPUTS) || type === 'search'));
// M3: with no interactive role, a clickable element (R53's rule) is a button named by its text,
// which Space or Enter presses as a click does; never a composer or a field (a role of its own
// on an editable element leaves it one: Enter types there).
const own = roleOf(e);
const role = !INTERACTIVE[own] && !e.isContentEditable && !field && clickable(e) ? 'button' : own;
const name = nameOf(e, !!BY_CONTENT[role]);
// A password's value is never read: the vault's fill handles logins.
const value = e.isContentEditable ? raw(e) : field && type !== 'password' ? (e.value || '') : '';
const facts = formFacts(formOf(e));
return JSON.stringify({
  role, name,
  placeholder: e.getAttribute('placeholder') || e.getAttribute('aria-placeholder') || '',
  editable: !!e.isContentEditable || field, value: String(value).slice(0, 4000),
  textarea: facts.textarea, password: facts.password,
  // Enter on a button presses the button; in any other control of a form, it submits the form.
  submit: role === 'button' ? null : facts.submit,
});"#;

/// What holds `sel`'s match (R53): the match or its nearest ancestor with an interactive role,
/// or one a click handler, a tab stop or its own pointer cursor makes clickable (a button named
/// by its text), as agent-browser's own scan finds them; `{role: ""}` with its text for none.
const HOLDER_BODY: &str = r#"const e = find(sel);
if (!e) return JSON.stringify({ unread: true });
for (let n = e, i = 0; n && i < 64; n = up(n), i++) {
  if (n.nodeType !== 1) continue;
  const role = roleOf(n);
  if (INTERACTIVE[role]) return JSON.stringify({ role, name: nameOf(n, !!BY_CONTENT[role]) });
  if (clickable(n)) return JSON.stringify({ role: 'button', name: nameOf(n, true) });
}
return JSON.stringify({ role: '', name: text(e).slice(0, 200) });"#;

/// The forms a field may be in (I3), each with its facts and default button: the field `sel`
/// matches, or with no selector the fields named `name` in the page and the frames it can reach
/// (all of their fields when none is), as a script cannot reach an element by ref; `{unread}`
/// when `sel` matches nothing.
const FORMS_BODY: &str = r#"const norm = s => String(s || '').replace(/\s+/g, ' ').trim().toLowerCase();
let fields = [];
if (sel !== null) {
  const e = find(sel);
  if (!e) return JSON.stringify({ unread: true });
  fields = [e];
} else {
  // The page's own document and those of the frames it can reach (a ref may be in one).
  const docs = [document];
  for (let i = 0; i < docs.length && docs.length < 32; i++) {
    for (const f of docs[i].querySelectorAll('iframe, frame')) {
      try { if (f.contentDocument) docs.push(f.contentDocument); } catch (_) {}
    }
  }
  const all = docs.flatMap(d => Array.from(d.querySelectorAll(
    'input, textarea, select, [contenteditable]:not([contenteditable="false"])')));
  const named = all.filter(e => norm(nameOf(e, false) || e.getAttribute('placeholder')) === norm(name));
  fields = named.length ? named : all;
}
const forms = [];
for (const f of fields) {
  const form = formOf(f);
  if (form && !forms.includes(form)) forms.push(form);
}
return JSON.stringify({ forms: forms.map(formFacts) });"#;

/// Whether a form holding a control named `submit` has a textarea (or an editable area).
const SUBMIT_BODY: &str = r#"const norm = s => String(s || '').replace(/\s+/g, ' ').trim().toLowerCase();
const named = e => norm(e.getAttribute('aria-label') || e.innerText || e.value || e.getAttribute('title'));
const all = Array.from(document.querySelectorAll('button, input[type="submit" i], input[type="button" i], ' +
  'input[type="image" i], [role="button"], [role="menuitem"]')).filter(e => named(e) === 'submit');
const forms = all.map(e => e.form || e.closest('form'));
const area = 'textarea, [contenteditable]:not([contenteditable="false"])';
return JSON.stringify({ found: all.length, textarea: forms.some(f => !!(f && f.querySelector(area))) });"#;

// ---- the snapshot's text ----

/// One line of agent-browser's snapshot (0.41 `render_tree`): `- role "name" [level=1, ref=e3]
/// clickable [cursor:pointer] : value`, two spaces of indent a level (a node agent-browser
/// skips leaves its children at its own), the name written as JSON.
struct Line {
    depth: usize,
    role: String,
    name: String,
    value: String,
    /// `e3` for `[…, ref=e3]`.
    reference: Option<String>,
    /// What agent-browser's scan for clickable elements with no role found (`clickable`,
    /// `focusable`, `editable`).
    cursor: Option<String>,
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
    let mut after = rest.trim_start();
    let mut reference = None;
    if let Some((attrs, more)) = (after.strip_prefix('[')).and_then(|a| a.split_once(']')) {
        let refs = attrs.split(", ").filter_map(|a| a.strip_prefix("ref="));
        reference = refs.last().map(String::from);
        after = more.trim_start();
    }
    let mut cursor = None;
    for kind in ["clickable", "focusable", "editable"] {
        if let Some(more) = (after.strip_prefix(kind)).and_then(|m| m.strip_prefix(" [")) {
            cursor = Some(kind.to_string());
            after = more.split_once(']').map_or("", |(_, m)| m);
        }
    }
    let value = after.split_once(": ").map_or("", |(_, v)| v);
    Some(Line {
        depth,
        role: role.trim_end_matches(':').to_string(),
        name,
        value: value.to_string(),
        reference,
        cursor,
    })
}

/// agent-browser 0.41's `shell_words_split` (cli/src/commands.rs), ported as it is: how it splits
/// each of a batch's commands. Words part at a space (only), outside quotes; `"` and `'` quote
/// and are dropped; a backslash outside single quotes keeps the next character as it is (and a
/// trailing one is dropped); an empty word is no word.
pub fn shell_words_split(s: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let (mut in_double, mut in_single) = (false, false);
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '\\' if !in_single => {
                if let Some(next) = chars.next() {
                    current.push(next);
                }
            }
            '"' if !in_single => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            ' ' if !in_double && !in_single => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    args
}

/// The page's labelled total (`total`, `order total`, `amount due`, `you pay`; Korean `합계`,
/// `총액`, `결제금액`), read from its snapshot: the amount after the label, in its text or in
/// one of the three texts that follow. Of several, the largest, so a `total savings` never
/// stands for the order's total; amounts that name a currency count over those that name none,
/// and totals in two currencies are no total. `$` reads as US dollars (every other dollar is
/// worth less, so a cap is never passed on that reading); `¥` names no currency (yen or yuan).
/// A number by a label that reads more than one way (`1 23,50`) makes it no total at all: an
/// unknown amount is over any cap (I5).
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
        let mut ahead = Vec::new();
        for n in next {
            ahead.extend(amounts(n)?);
        }
        let marked = ahead.iter().find(|a| !a.currency.is_empty()).cloned();
        found.extend(
            money(&after)?
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

/// The first amount in `text` that names its currency, else its first number; `None` when a
/// number there reads more than one way.
fn money(text: &str) -> Option<Option<PageAmount>> {
    let all = amounts(text)?;
    let marked = all.iter().find(|a| !a.currency.is_empty()).cloned();
    Some(marked.or_else(|| all.into_iter().next()))
}

/// What may group a number's thousands besides `,` and `.`: a space, a no-break space, a narrow
/// or thin one, an apostrophe (`1 234,50`, `1'234.50`), each only before exactly three digits.
const GROUPING: &[char] = &[' ', '\u{a0}', '\u{202f}', '\u{2009}', '\'', '\u{2019}'];

/// Every number in `text`, each with the currency written beside it (empty for none); `None`
/// when one reads more than one way ([`number_value`]).
fn amounts(text: &str) -> Option<Vec<PageAmount>> {
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
                || ((matches!(chars[i], ',' | '.') || GROUPING.contains(&chars[i]))
                    && chars.get(i + 1).is_some_and(char::is_ascii_digit)))
        {
            i += 1;
        }
        let number: String = chars[start..i].iter().collect();
        let before: String = chars[..start].iter().collect();
        let after: String = chars[i..].iter().collect();
        let value = number_value(&number)?;
        let currency = currency(before.trim_end(), after.trim_start());
        out.push(PageAmount { value, currency });
    }
    Some(out)
}

/// `1,234.56`, `1.234,56`, `1 234,56`, `1'234.56`, `54,200`, `54.20`: the last `,` or `.` is the
/// decimal point when one or two digits (or more than three) follow it; with exactly three it
/// groups thousands, the larger reading. Every other separator groups thousands: all one kind,
/// not the decimal point's, with one to three digits before the first and exactly three after
/// each. Anything else (`1 23,50`, `1.23.45`, `12 3456`, India's `1,23,456`) reads more than one
/// way: `None`.
fn number_value(number: &str) -> Option<f64> {
    let mut groups = vec![String::new()];
    let mut seps = Vec::new();
    for c in number.chars() {
        match c.is_ascii_digit() {
            true => groups.last_mut()?.push(c),
            false => {
                seps.push(c);
                groups.push(String::new());
            }
        }
    }
    let last = groups.last()?;
    let decimal = match seps.last() {
        Some(&s @ (',' | '.')) if last.len() != 3 => Some(s),
        _ => None,
    };
    let whole = groups.len() - usize::from(decimal.is_some());
    let grouping = &seps[..whole - 1];
    if let Some(&g) = grouping.first() {
        let one_kind = grouping.iter().all(|&s| s == g) && Some(g) != decimal;
        let sizes =
            (1..=3).contains(&groups[0].len()) && groups[1..whole].iter().all(|g| g.len() == 3);
        if !(one_kind && sizes) {
            return None;
        }
    }
    let int = groups[..whole].concat();
    let text = match decimal {
        Some(_) => format!("{int}.{last}"),
        None => int,
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
    const AFTER: &[(&str, &str)] = &[
        ("원", "KRW"),
        ("₩", "KRW"),
        ("€", "EUR"),
        ("円", "JPY"),
        ("元", "CNY"),
    ];
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
    /// (failing when there is none); anything else prints `done: <argv>`. A scoped snapshot
    /// (`-s`) reads `scoped.json` when there is one; an eval reads `eval-<kind>.json` for the
    /// script whose first line is `/*zephyr:<kind>*/`, when there is one.
    const FAKE: &str = r#"#!/bin/sh
d=$(dirname "$0")
printf '%s\n' "$*" >> "$d/argv.log"
case " $* " in
  *" get url "*) f=url ;;
  *" get title "*) f=title ;;
  *" snapshot "*) f=snapshot
    case " $* " in *" -s "*) [ -f "$d/scoped.json" ] && f=scoped ;; esac ;;
  *" eval "*) f=eval
    prev=; for a in "$@"; do [ "$prev" = "-b" ] && b=$a; prev=$a; done
    k=$(printf '%s' "$b" | base64 -d 2>/dev/null | sed -n '1s|^/\*zephyr:\([a-z]*\)\*/.*|\1|p')
    [ -n "$k" ] && [ -f "$d/eval-$k.json" ] && f=eval-$k ;;
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

        /// What the wrapper's `kind` script (`/*zephyr:<kind>*/`) reports.
        fn eval(&self, kind: &str, facts: Value) {
            let name = format!("eval-{kind}.json");
            self.file(&name, json!({"result": facts.to_string(), "origin": "x"}));
        }

        /// What a snapshot scoped to a selector shows: `lines` (role, name), top-level, no refs.
        fn scoped(&self, lines: &[(&str, &str)]) {
            let tree: Vec<String> = (lines.iter())
                .map(|(role, name)| format!("- {role} {}", Value::from(*name)))
                .collect();
            let data = json!({"snapshot": tree.join("\n"), "origin": MAIL, "refs": {}});
            self.file("scoped.json", data);
        }

        /// The wrapper's snapshots, in the order it took them.
        fn snapshots(&self) -> Vec<String> {
            let all = self.argv().into_iter();
            all.filter(|l| l.contains("snapshot")).collect()
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

    /// Review Focus 3: while the owner holds the desktop, neither wrapper reads the page or runs
    /// the real binary; each waits on the gate and exits 1 with its words.
    #[test]
    fn a_held_desktop_stops_agent_browser_and_xdotool_before_the_real_binary() {
        let t = T::new("bg-held");
        t.page(MAIL, COMPOSE, "");
        std::fs::write(t.dir.join(crate::desktop::HELD_FILE), "100\n").unwrap();
        let gate = t.gate();
        let back = "zephyr: the owner used the desktop from 14:04 to 14:08; this command did \
                    not run. The page may have changed: take a fresh snapshot before acting.";
        // One at a time: each answer goes to the wrapper that asked.
        for which in 0..3 {
            let env = t.env.clone();
            let waiting = match which {
                0 => t.spawn(&["snapshot", "-i"]),
                1 => t.spawn(&["click", "@e4"]),
                _ => std::thread::spawn(move || xdotool(&env)),
            };
            let (req, mut conn) = asked(&gate);
            assert_eq!(req, json!({"v": 1, "desktop": "wait", "session": "main"}));
            answer(&mut conn, json!({"verdict": "deny", "text": back}));
            assert_eq!(waiting.join().unwrap(), Next::Refuse(back.into()));
        }
        // An `allow` is not the gate's answer to a wait: nothing runs on it.
        let waiting = t.spawn(&["open", "https://example.com"]);
        answer(&mut asked(&gate).1, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), Next::Refuse(NOT_ANSWERING.into()));
        assert!(t.argv().is_empty(), "the real binary ran: {:?}", t.argv());
        // Help and the version touch no page.
        assert_eq!(t.run(&["--version"]), Next::Exec);
        assert_eq!(t.run(&["click", "@e3", "--help"]), Next::Exec);
        assert!(gate.try_recv().is_err());
        // Handed back between the wrapper's look and the gate's: an empty text, and it goes on.
        let waiting = t.spawn(&["snapshot", "-i"]);
        answer(&mut asked(&gate).1, json!({"verdict": "deny", "text": ""}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
    }

    #[test]
    fn with_no_flag_both_run_as_before() {
        let t = T::new("bg-not-held");
        t.page(MAIL, COMPOSE, "");
        let gate = t.gate();
        assert_eq!(t.run(&["snapshot", "-i"]), Next::Exec);
        assert_eq!(xdotool(&t.env), Next::Exec);
        assert!(gate.try_recv().is_err(), "the gate was asked");
        assert!(held(&t.env).is_none());
    }

    #[test]
    fn held_with_no_gate_is_refused() {
        let t = T::new("bg-held-alone");
        std::fs::write(t.dir.join(crate::desktop::HELD_FILE), "100\n").unwrap();
        // No gate listens on the socket.
        assert_eq!(
            t.run(&["snapshot", "-i"]),
            Next::Refuse(NOT_ANSWERING.into())
        );
        assert_eq!(xdotool(&t.env), Next::Refuse(NOT_ANSWERING.into()));
        assert!(t.argv().is_empty());
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
            (
                "deny",
                "not run: 3 actions already wait for the owner in this run",
            ),
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
        // Enter goes unasked in an empty search box, a box whose placeholder says search (A1), a
        // login form, a link and the page itself (with text in them, it is asked about: I1).
        for facts in [
            json!({"role": "searchbox", "name": "", "editable": true, "value": ""}),
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
        t.eval("forms", json!({"forms": []}));
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
            &["batch", "--bail", "press Enter"],
            // In a batch, `--help` is no help: the click would run.
            &["batch", "click @e3 --help"],
            &["batch", "\"click\" @e3"],
            // C1: each command is split as agent-browser 0.41 splits it (`shell_words_split`).
            &["batch", r#"find text "Send message""#],
            &["batch", "find text 'Send message' click"],
            &["batch", r#"find label "Card number" fill 4111 1111"#],
            &["batch", r#"cl"ic"k @e3"#],
            &["batch", r"cl\ick @e3"],
            &["batch", r#"type @e2 "hi\ there""#],
            &["batch", "keyboard type 'a\nb'"],
            // Inside a batch no flag is taken out (0.41 parses each command as it stands), and
            // either reading that needs the gate refuses the batch.
            &["batch", "type @e2 --session \"\n\""],
            &["batch", "--session s click @e3"],
            &["batch", "batch 'click @e3'"],
            &["batch"],
            &["batch", "--bail"],
            &["mcp"],
            &["mcp", "call", "agent_browser_click", "{}"],
            &["chat", "send the mail"],
            // R55: a page's own tools act past the gate too.
            &["webmcp", "invoke", "send_email", "--params", "{}"],
            &["--session", "s", "webmcp", "invoke", "checkout"],
            &["batch", "webmcp invoke checkout"],
        ] {
            assert!(matches!(t.run(args), Next::Refuse(_)), "{args:?}");
        }
        // What agent-browser runs nothing for, or only reads, passes: 0.41 reads no JSON in a
        // batch's arguments (`[click,` is no command), skips an empty command, and lists or
        // reads a page's tools without calling one.
        for args in [
            &["batch", r#"["click", "@e3"]"#][..],
            &["batch", "snapshot -i", "get text @e1", ""],
            &["webmcp", "list"],
            &["webmcp", "result", "inv-1"],
        ] {
            assert_eq!(t.run(args), Next::Exec, "{args:?}");
        }
        assert!(t.argv().is_empty());
    }

    /// C1: agent-browser 0.41's `shell_words_split` (commands.rs), which splits a batch's
    /// commands: spaces only, single and double quotes, a backslash escaping the next character
    /// outside single quotes, and no empty word.
    #[test]
    fn batch_commands_split_as_agent_browser_splits_them() {
        for (text, want) in [
            (
                r#"find text "Send message""#,
                &["find", "text", "Send message"][..],
            ),
            (r#"cl"ic"k @e3"#, &["click", "@e3"]),
            (r"cl\ick  @e3 ", &["click", "@e3"]),
            ("a 'b \"c\" d' e", &["a", "b \"c\" d", "e"]),
            (r#""a \"b\"" 'c\d'"#, &["a \"b\"", r"c\d"]),
            ("tab\tstays one", &["tab\tstays", "one"]),
            (r#"x "" y"#, &["x", "y"]),
            (r"end\", &["end"]),
            ("", &[]),
        ] {
            assert_eq!(shell_words_split(text), want, "{text}");
        }
    }

    /// C2: Enter however it is written: a typed line break, a `\n` or `\r` key, a chord ending
    /// in Enter, a key going up as well as down.
    #[test]
    fn enter_however_it_is_written_is_a_key_press() {
        let t = T::new("bg-enter");
        t.page(MAIL, COMPOSE, "");
        t.focus(json!({"role": "textbox", "name": "Message", "editable": true, "value": "hi"}));
        t.eval("forms", json!({"forms": []}));
        let gate = t.gate();
        for args in [
            &["type", "@e2", "\n"][..],
            &["type", "@e2", "\r"],
            &["type", "@e2", "--delay", "5", "ok\r"],
            &["keyboard", "type", "\n"],
            &["press", "\n"],
            &["press", "\r"],
            &["key", "Shift+\n"],
            &["press", "ENTER"],
            &["keydown", "Return"],
            &["keyup", "Enter"],
            // N2: 0.41 takes the modifier words out wherever they stand (`parse_key_chord`).
            &["press", "Enter+Control"],
            &["press", "Return+Meta"],
            &["press", "Enter+Shift"],
            &["press", "\r+Control"],
            &["press", "Control+Shift+Enter"],
            &["press", "ctrl+enter+alt"],
            &["keydown", "Enter+Meta"],
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["name"].as_str()),
                (Some("press"), Some("Message")),
                "{args:?}: {req}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
        }
        // A space or a tab typed or pressed in a composer is no send; nor is a chord with no
        // modifier word, which 0.41 sends whole as a key of that name (`a+Enter`).
        for args in [
            &["type", "@e2", " "][..],
            &["keyboard", "type", "  "],
            &["press", "Tab"],
            &["press", "Space"],
            &["press", "a+Enter"],
            &["press", "Control+a"],
            &["press", "Enter+"],
        ] {
            assert_eq!(t.run(args), Next::Exec, "{args:?}");
        }
        assert!(gate.try_recv().is_err());
    }

    /// N2: agent-browser 0.41's own `parse_key_chord` tests (native/actions.rs), and modifiers
    /// after the key.
    #[test]
    fn a_key_chord_is_read_as_agent_browser_reads_it() {
        for (input, key, mods) in [
            ("a", "a", None),
            ("Enter", "Enter", None),
            ("Control+a", "a", Some(2)),
            ("Ctrl+c", "c", Some(2)),
            ("Shift+Enter", "Enter", Some(8)),
            ("Control+Shift+a", "a", Some(2 | 8)),
            ("Meta+a", "a", Some(4)),
            ("Alt+Tab", "Tab", Some(1)),
            ("+", "+", None),
            ("Enter+Control", "Enter", Some(2)),
            ("\r+Control", "\r", Some(2)),
            ("cmd+Return+SHIFT", "Return", Some(4 | 8)),
            ("a+Enter", "a+Enter", None),
            ("Control+a+Enter", "a+Enter", Some(2)),
            ("Control+", "", Some(2)),
        ] {
            assert_eq!(parse_key_chord(input), (key.to_string(), mods), "{input:?}");
        }
    }

    /// I1: what is typed before a line break goes out with it, so it is checked for private
    /// facts even where Enter sends nothing the gate holds by class.
    #[test]
    fn typing_with_a_line_break_is_checked_for_facts_wherever_it_goes() {
        let t = T::new("bg-typed-enter");
        t.page(MAIL, &[("searchbox", "Search mail")], "");
        t.eval("forms", json!({"forms": []}));
        let gate = t.gate();
        for (args, typed) in [
            (&["type", "#q", "12 Main St\n"][..], "12 Main St\n"),
            (&["type", "@e1", "010-2345-6789\r"], "010-2345-6789\r"),
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["role"].as_str()),
                (Some("press"), Some("searchbox")),
                "{args:?}: {req}"
            );
            assert_eq!(b["typed"], json!([typed]), "{args:?}");
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
        }
        // Enter in a search box holding text: its text goes out.
        t.focus(json!({"role": "searchbox", "name": "", "editable": true, "value": "my address"}));
        let waiting = t.spawn(&["press", "Enter"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["typed"], json!(["my address"]), "{req}");
        answer(&mut conn, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        // Nothing typed but the line break, where Enter sends nothing: nothing to ask.
        assert_eq!(t.run(&["type", "@e1", "\n"]), Next::Exec);
        assert!(gate.try_recv().is_err());
    }

    /// N3, R57: 0.41 types each `\t` as a real Tab, which moves the focus, and an Enter can
    /// send and move it too: a line break after a Tab, and any key after the first Enter, land
    /// on a control the wrapper did not read, which the gate holds.
    #[test]
    fn keys_after_a_tab_or_an_enter_land_where_the_wrapper_did_not_read() {
        let t = T::new("bg-tabs");
        t.page(
            MAIL,
            &[("searchbox", "Search mail"), ("textbox", "Message")],
            "",
        );
        t.focus(json!({"role": "document", "name": ""}));
        t.eval("forms", json!({"forms": []}));
        let gate = t.gate();
        for (args, typed) in [
            (&["keyboard", "type", "\t\t\n"][..], "\t\t\n"),
            (&["type", "@e1", "\t\n"], "\t\n"),
            (&["type", "@e1", "x\t\n"], "x\t\n"),
            (&["type", "@e2", "hi\t\n"], "hi\t\n"),
            (&["type", "@e2", "ok\n\n"], "ok\n\n"),
            (&["type", "@e2", "ok\r\n"], "ok\r\n"),
            (&["type", "@e2", "ok\n\t"], "ok\n\t"),
            (&["keyboard", "type", "a\rb\r"], "a\rb\r"),
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["role"].as_str(), b["name"].as_str()),
                (Some("press"), Some(""), Some("")),
                "{args:?}: {req}"
            );
            assert_eq!(b["typed"], json!([typed]), "{args:?}");
            answer(&mut conn, json!({"verdict": "hold", "text": "held"}));
            assert_eq!(
                waiting.join().unwrap(),
                Next::Refuse("held".into()),
                "{args:?}"
            );
        }
        // One Enter, with only text around it (typed in, not keys), lands where it was read.
        for args in [&["type", "@e2", "ok\n"][..], &["type", "@e2", "ok\n  "]] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["role"].as_str(), b["name"].as_str()),
                (Some("textbox"), Some("Message")),
                "{args:?}: {req}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
        }
        // Tabs with no line break send nothing: what is typed is checked for facts only.
        let waiting = t.spawn(&["type", "@e2", "a\tb"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["action"], "type", "{req}");
        answer(&mut conn, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        assert_eq!(t.run(&["keyboard", "type", "\t\t"]), Next::Exec);
        assert!(gate.try_recv().is_err());
    }

    /// B7: agent-browser 0.41 focuses a `type` target without checking that it took the focus,
    /// so a line break typed into a control that cannot take text lands wherever the focus was:
    /// the wrapper cannot read where, and holds it as unread (R57).
    #[test]
    fn a_line_break_typed_into_a_control_that_takes_no_text_is_blind() {
        let t = T::new("bg-no-focus");
        t.page(
            MAIL,
            &[
                ("link", "Inbox"),
                ("checkbox", "Remember me"),
                ("textbox", "Message"),
                ("combobox", "To"),
            ],
            "",
        );
        t.focus(json!({"role": "textbox", "name": "Message"}));
        t.eval("forms", json!({"forms": []}));
        let gate = t.gate();
        for target in ["@e1", "@e2"] {
            let waiting = t.spawn(&["type", target, "hi\n"]);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["role"].as_str(), b["name"].as_str()),
                (Some("press"), Some(""), Some("")),
                "{target}: {req}"
            );
            assert_eq!(b["typed"], json!(["hi\n"]), "{target}");
            answer(&mut conn, json!({"verdict": "hold", "text": "held"}));
            assert_eq!(waiting.join().unwrap(), Next::Refuse("held".into()));
        }
        // A field that takes text is where the line break lands, as read.
        for (target, role, name) in [("@e3", "textbox", "Message"), ("@e4", "combobox", "To")] {
            let waiting = t.spawn(&["type", target, "hi\n"]);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["role"].as_str(), b["name"].as_str()),
                (Some(role), Some(name)),
                "{target}: {req}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{target}");
        }
    }

    /// N1: a button pressed by a key keeps its form's facts, as a click on it is classed: a
    /// plain `submit` in a form with a textarea posts.
    #[test]
    fn a_pressed_button_keeps_its_forms_facts() {
        let t = T::new("bg-pressed");
        t.page(MAIL, &[("button", "Submit")], "");
        let gate = t.gate();
        let posts = |args: &[&str], why: &str| {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["role"].as_str(), b["name"].as_str()),
                (Some("click"), Some("button"), Some("Submit")),
                "{args:?} {why}: {req}"
            );
            assert_eq!(b["form_textarea"], true, "{args:?} {why}: {req}");
            assert_eq!(
                gate::browser_class(&serde_json::from_value(b.clone()).unwrap()),
                Some(Class::Posts),
                "{args:?} {why}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?} {why}");
        };
        // The focus script read the button's form: it has a textarea.
        t.focus(json!({"role": "button", "name": "Submit", "textarea": true}));
        t.eval("submit", json!({"found": 1, "textarea": false}));
        for args in [
            &["press", "Enter"][..],
            &["press", "Space"],
            &["keyup", "Enter"],
        ] {
            posts(args, "(its form read with the focus)");
        }
        // Not read with the focus: the page is asked, as for a click.
        t.focus(json!({"role": "button", "name": "Submit"}));
        t.eval("submit", json!({"found": 1, "textarea": true}));
        posts(&["press", "Enter"], "(its form asked about)");
        posts(&["type", "@e1", "\n"], "(a line break typed into it)");
        // A Submit in a form with no textarea is no post.
        t.eval("submit", json!({"found": 1, "textarea": false}));
        assert_eq!(t.run(&["press", "Enter"]), Next::Exec);
        assert_eq!(t.run(&["type", "@e1", "\n"]), Next::Exec);
        assert!(gate.try_recv().is_err());
        // Round 3: a password field in its form leaves a pressed button classed as a click on
        // it is (a click never reads its form's password field): a Pay in a checkout with an
        // account password is a payment, a Send in a transfer form with a confirmation one a
        // message.
        for (name, class) in [("Pay", Class::Money), ("Send", Class::Messages)] {
            t.focus(json!({"role": "button", "name": name, "password": true}));
            let waiting = t.spawn(&["press", "Enter"]);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["name"].as_str()),
                (Some("click"), Some(name)),
                "{req}"
            );
            assert_eq!(b["form_password"], false, "as a click on it: {req}");
            assert_eq!(
                gate::browser_class(&serde_json::from_value(b.clone()).unwrap()),
                Some(class),
                "{name}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{name}");
        }
    }

    /// I3: Enter in a form's field submits it through its default button, which the wrapper
    /// reads and the gate classes with the field; Space or Enter on a focused button presses it.
    #[test]
    fn a_key_press_counts_what_it_submits_or_presses() {
        let t = T::new("bg-submit");
        let checkout = "https://shop.example.com/checkout";
        let total = "- StaticText \"Total\"\n- StaticText \"$54.20\"";
        t.page(
            checkout,
            &[("textbox", "Coupon code"), ("button", "Apply")],
            total,
        );
        t.scoped(&[("textbox", "Coupon code")]);
        t.file("title.json", json!({"title": "Checkout"}));
        let gate = t.gate();
        let order = json!({"role": "button", "name": "Place order"});
        t.focus(
            json!({"role": "textbox", "name": "Coupon code", "editable": true,
                       "value": "SAVE10", "submit": order}),
        );
        let waiting = t.spawn(&["press", "Enter"]);
        let (req, mut conn) = asked(&gate);
        let b = &req["browser"];
        assert_eq!(b["submit"], order, "{req}");
        assert_eq!(
            b["title"], "Checkout",
            "a payment: its title and total are read"
        );
        assert_eq!(b["amount"], json!({"value": 54.2, "currency": "USD"}));
        answer(&mut conn, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        // A line break typed there does the same: the field found by its selector, or by name.
        let apply = json!({"submit": {"role": "button", "name": "Apply"}});
        t.eval("forms", json!({"forms": [apply, {"submit": order}]}));
        for args in [
            &["type", "#coupon", "SAVE10\n"][..],
            &["type", "@e1", "SAVE10\n"],
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            assert_eq!(req["browser"]["submit"], order, "{args:?}: {req}");
            assert_eq!(req["browser"]["title"], "Checkout", "{args:?}");
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
        }
        // What the form submits cannot be read: the control is one the gate could not read.
        std::fs::remove_file(t.dir.join("eval-forms.json")).unwrap();
        let waiting = t.spawn(&["type", "@e1", "SAVE10\n"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["role"], "", "{req}");
        answer(&mut conn, json!({"verdict": "hold", "text": "held"}));
        assert_eq!(waiting.join().unwrap(), Next::Refuse("held".into()));
        // A login form's default button sends nothing.
        t.focus(
            json!({"role": "textbox", "name": "Email", "editable": true, "password": true,
                       "submit": {"role": "button", "name": "Send code"}}),
        );
        assert_eq!(t.run(&["press", "Enter"]), Next::Exec);
        // R58: a checkout with an optional account password still pays.
        t.focus(
            json!({"role": "textbox", "name": "Coupon code", "editable": true, "password": true,
                   "submit": order}),
        );
        let waiting = t.spawn(&["press", "Enter"]);
        let (req, mut conn) = asked(&gate);
        let b = req["browser"].clone();
        assert_eq!(
            (b["form_password"].clone(), b["submit"].clone()),
            (json!(true), order.clone())
        );
        assert_eq!(
            gate::browser_class(&serde_json::from_value(b).unwrap()),
            Some(Class::Money)
        );
        answer(&mut conn, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        // Space or Enter on a focused button is a click on it.
        t.focus(json!({"role": "button", "name": "Send"}));
        for args in [
            &["press", "Space"][..],
            &["press", " "],
            &["key", "Enter"],
            &["keyup", "Space"],
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["action"].as_str(), b["role"].as_str(), b["name"].as_str()),
                (Some("click"), Some("button"), Some("Send")),
                "{args:?}: {req}"
            );
            assert_eq!(b["composer"], false, "{args:?}");
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
        }
        // Space on a link, in a composer or on the page is no press of a button.
        for facts in [
            json!({"role": "link", "name": "Send"}),
            json!({"role": "textbox", "name": "Message", "editable": true, "value": "hi"}),
            json!({"role": "document", "name": ""}),
        ] {
            t.focus(facts.clone());
            assert_eq!(t.run(&["press", "Space"]), Next::Exec, "{facts}");
        }
        assert!(gate.try_recv().is_err());
        // A focus the wrapper cannot read may be on a button: Space is asked about too.
        t.focus(json!({"unread": true}));
        let waiting = t.spawn(&["press", "Space"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["role"], "", "{req}");
        answer(&mut conn, json!({"verdict": "hold", "text": "held"}));
        assert_eq!(waiting.join().unwrap(), Next::Refuse("held".into()));
    }

    /// I4: after the owner's answer, the control (found by a ref, a selector, a locator or the
    /// focus) is read again, and the action goes only if it is still the one the owner saw, on
    /// the same site, and for a payment with the same total.
    #[test]
    fn the_page_is_read_again_before_an_approved_click() {
        let t = T::new("bg-recheck");
        let gate = t.gate();
        let changed = Next::Refuse(CHANGED.into());
        for (args, change) in [
            (&["click", "#send"][..], "name"),
            (&["click", "#send"], "role"),
            (&["click", "#send"], "origin"),
            (&["find", "text", "Send"], "name"),
            (
                &["find", "role", "button", "click", "--name", "Send"],
                "origin",
            ),
            (&["click", "#send"], "nothing"),
            (&["find", "text", "Send"], "nothing"),
        ] {
            t.page(MAIL, &[("button", "Send")], "");
            let waiting = t.spawn(args);
            let (_, mut conn) = asked(&gate);
            match change {
                "name" => t.page(MAIL, &[("button", "Send all")], ""),
                "role" => t.page(MAIL, &[("menuitem", "Send")], ""),
                "origin" => t.page("https://evil.example.org/x", &[("button", "Send")], ""),
                _ => {}
            }
            answer(&mut conn, json!({"verdict": "allow"}));
            let want = match change {
                "nothing" => Next::Exec,
                _ => changed.clone(),
            };
            assert_eq!(waiting.join().unwrap(), want, "{args:?}: {change}");
        }
        // A key press: the focused element, the text it holds and what its form submits.
        let composer = json!({"role": "textbox", "name": "Message", "editable": true,
                              "value": "hi bob"});
        for now in [
            json!({"role": "textbox", "name": "Search mail", "editable": true, "value": "hi bob"}),
            json!({"role": "textbox", "name": "Message", "editable": true,
                   "value": "and the card number"}),
            json!({"role": "textbox", "name": "Message", "editable": true, "value": "hi bob",
                   "submit": {"role": "button", "name": "Pay"}}),
        ] {
            t.page(MAIL, COMPOSE, "");
            t.focus(composer.clone());
            let waiting = t.spawn(&["press", "Enter"]);
            let (_, mut conn) = asked(&gate);
            t.focus(now.clone());
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), changed, "{now}");
        }
        // M1: a ref is read again too, in one full snapshot: a node gone falls back to another
        // with its role and name (0.41 element.rs), and a live one may be relabelled.
        let relabelled: &[(&str, &str)] =
            &[("textbox", "To"), ("textbox", "Message"), ("button", "Pay")];
        let gone: &[(&str, &str)] = &[("textbox", "To"), ("textbox", "Message")];
        for (now, want) in [
            (&[("button", "Delete account")][..], changed.clone()),
            (relabelled, changed.clone()),
            (gone, changed.clone()),
            (COMPOSE, Next::Exec),
        ] {
            t.page(MAIL, COMPOSE, "");
            let waiting = t.spawn(&["click", "@e3"]);
            let (_, mut conn) = asked(&gate);
            let reads = t.argv().len();
            t.page(MAIL, now, "");
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), want, "{now:?}");
            assert_eq!(
                t.argv()[reads..],
                ["get url --json", "snapshot --json"],
                "{now:?}"
            );
        }
        // ... and a ref on a page that went to another site clicks nothing there.
        t.page(MAIL, COMPOSE, "");
        let waiting = t.spawn(&["click", "@e3"]);
        let (_, mut conn) = asked(&gate);
        t.page("https://evil.example.org/x", COMPOSE, "");
        answer(&mut conn, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), changed);
        // A payment's total is read again: the owner approved the amount the card showed.
        let shop = "https://shop.example.com/cart";
        for (later, want) in [("$54.20", Next::Exec), ("$540.20", changed.clone())] {
            t.page(shop, &[("button", "Pay")], "- StaticText \"Total $54.20\"");
            let waiting = t.spawn(&["click", "@e1"]);
            let (_, mut conn) = asked(&gate);
            let tree = format!("- StaticText \"Total {later}\"");
            t.page(shop, &[("button", "Pay")], &tree);
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), want, "{later}");
        }
        assert!(!t.clicked());
    }

    /// M2: an action the owner let go (an `allow` with its number, after a wait) tells the gate
    /// whether it ran, on the same connection, once the page is read again: `{"ran": true}`
    /// just before it runs, `{"ran": false}` when the page changed. One the gate cannot be told
    /// of is not run. An `allow` with no wait behind it is told nothing.
    #[test]
    fn a_let_go_action_tells_the_gate_whether_it_ran() {
        let t = T::new("bg-report");
        let gate = t.gate();
        let said = |conn: &mut UnixStream| {
            conn.set_read_timeout(Some(Duration::from_secs(20)))
                .unwrap();
            let mut line = String::new();
            let _ = BufReader::new(conn.try_clone().unwrap()).read_line(&mut line);
            line
        };
        for (moved, want, report) in [
            (false, Next::Exec, "{\"ran\":true}\n"),
            (true, Next::Refuse(CHANGED.into()), "{\"ran\":false}\n"),
        ] {
            t.page(MAIL, COMPOSE, "");
            let waiting = t.spawn(&["click", "@e3"]);
            let (_, mut conn) = asked(&gate);
            if moved {
                t.page("https://evil.example.org/x", COMPOSE, "");
            }
            answer(&mut conn, json!({"verdict": "allow", "id": 7}));
            assert_eq!(said(&mut conn), report, "moved: {moved}");
            assert_eq!(waiting.join().unwrap(), want, "moved: {moved}");
        }
        // Typing let go after a wait says so too.
        let waiting = t.spawn(&["fill", "@e1", "bob@example.com"]);
        let (_, mut conn) = asked(&gate);
        answer(&mut conn, json!({"verdict": "allow", "id": 8}));
        assert_eq!(said(&mut conn), "{\"ran\":true}\n");
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        // No wait behind the `allow`: nothing is said (the gate has hung up).
        t.page(MAIL, COMPOSE, "");
        let waiting = t.spawn(&["click", "@e3"]);
        let (_, mut conn) = asked(&gate);
        answer(&mut conn, json!({"verdict": "allow"}));
        assert_eq!(waiting.join().unwrap(), Next::Exec);
        assert_eq!(said(&mut conn), "");
        // A gate that stopped listening cannot be told that it ran: not run.
        let waiting = t.spawn(&["click", "@e3"]);
        let (_, mut conn) = asked(&gate);
        conn.shutdown(std::net::Shutdown::Read).unwrap();
        answer(&mut conn, json!({"verdict": "allow", "id": 9}));
        assert_eq!(waiting.join().unwrap(), Next::Refuse(NOT_ANSWERING.into()));
        assert!(!t.clicked());
    }

    /// M3, R53: the focus script reads a focused element with no interactive role that a click
    /// handler, a tab stop or its own pointer cursor makes clickable as a button named by its
    /// text, so Space or Enter on it is a click on it, as a click there is. Run in node against
    /// a stand-in for the page (skipped where there is no node).
    #[test]
    fn the_focus_script_reads_a_clickable_element_as_a_button() {
        let Ok(out) = Command::new("node").arg("--version").output() else {
            eprintln!("skipped: no node");
            return;
        };
        assert!(out.status.success());
        let dir = crate::test_dir("bg-focus-js");
        let page = r#"
const el = (tag, attrs, props) => Object.assign({
  tagName: tag.toUpperCase(), nodeType: 1, shadowRoot: null, isContentEditable: false,
  onclick: null, labels: null, innerText: '', textContent: '', parentElement: null,
  parentNode: null, form: null, closest: () => null,
  getAttribute: k => (k in attrs ? String(attrs[k]) : null), hasAttribute: k => k in attrs,
}, props);
const body = el('body', {}, {});
globalThis.document = { body, documentElement: el('html', {}, {}), activeElement: body,
  getElementById: () => null, querySelectorAll: () => [] };
globalThis.getComputedStyle = n => ({ cursor: n.pointer ? 'pointer' : 'auto' });
const cases = {
  onclick: el('div', { onclick: 'go()' }, { innerText: 'Send' }),
  handler: el('div', { tabindex: '-1' }, { innerText: 'Send', onclick: () => {} }),
  tabstop: el('div', { tabindex: '0' }, { innerText: 'Send' }),
  pointer: el('span', { tabindex: '-1' }, { innerText: 'Send', pointer: true }),
  plain: el('div', { tabindex: '-1' }, { innerText: 'Send' }),
  button: el('button', {}, { innerText: 'Send' }),
  // Round 3: a composer or a field with a role of its own stays what it is: Enter types there.
  editable: el('div', { contenteditable: 'true', role: 'presentation', tabindex: '0' },
    { isContentEditable: true, innerText: 'Send it now' }),
  field: el('input', { role: 'presentation', onclick: 'go()' }, { value: 'hi' }),
};
for (const n of Object.values(cases)) { n.ownerDocument = document; n.parentElement = body; }
const script = require('fs').readFileSync(process.argv[2], 'utf8');
const out = {};
for (const [k, n] of Object.entries(cases)) {
  document.activeElement = n;
  const v = JSON.parse(eval(script));
  out[k] = [v.role, v.name];
}
console.log(JSON.stringify(out));
"#;
        std::fs::write(dir.join("page.js"), page).unwrap();
        std::fs::write(dir.join("focus.js"), script("focus", "", FOCUS_BODY)).unwrap();
        let out = Command::new("node")
            .arg(dir.join("page.js"))
            .arg(dir.join("focus.js"))
            .output()
            .unwrap();
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let got: Value = serde_json::from_slice(&out.stdout).unwrap();
        let button = json!(["button", "Send"]);
        assert_eq!(
            got,
            json!({"onclick": button, "handler": button, "tabstop": button,
                   "pointer": button, "plain": ["generic", ""], "button": button,
                   "editable": ["presentation", ""], "field": ["presentation", ""]}),
        );
    }

    /// R53: a click lands on what holds its target: a heading in a Send button is the button's,
    /// a clickable element with no role is a button named by its text, a heading in a link is
    /// the link's, and a target nothing clickable holds is one the gate could not read.
    #[test]
    fn a_click_inside_a_button_is_the_buttons() {
        let t = T::new("bg-inside");
        let gate = t.gate();
        let tree = "- form \"Compose\"\n  - button \"Send\" [ref=e1]\n    - heading \"Now\" [level=3, ref=e2]\n\
                    - generic [ref=e3] clickable [cursor:pointer, onclick]\n  - StaticText \"Post it\"\n\
                    - heading \"Inbox\" [level=1, ref=e4]\n\
                    - link \"Home\" [ref=e5]\n  - heading \"Send\" [level=2, ref=e6]";
        let refs = json!({
            "e1": {"role": "button", "name": "Send"}, "e2": {"role": "heading", "name": "Now"},
            "e3": {"role": "generic", "name": ""}, "e4": {"role": "heading", "name": "Inbox"},
            "e5": {"role": "link", "name": "Home"}, "e6": {"role": "heading", "name": "Send"},
        });
        t.file("url.json", json!({"url": MAIL}));
        t.file(
            "snapshot.json",
            json!({"snapshot": tree, "origin": MAIL, "refs": refs}),
        );
        for (args, role, name) in [
            (&["click", "@e2"][..], "button", "Send"),
            (&["find", "text", "Now"], "button", "Send"),
            (&["click", "@e3"], "button", "Post it"),
            (&["click", "@e4"], "", "Inbox"),
        ] {
            let waiting = t.spawn(args);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["role"].as_str(), b["name"].as_str()),
                (Some(role), Some(name)),
                "{args:?}: {req}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
        }
        // A heading named Send, in a link: the link's, which sends nothing.
        assert_eq!(t.run(&["click", "@e6"]), Next::Exec);
        assert!(gate.try_recv().is_err());
        // A selector's target with no role: the page says what holds it.
        t.scoped(&[("StaticText", "Send")]);
        for (holder, role, name) in [
            (
                Some(json!({"role": "button", "name": "Send"})),
                "button",
                "Send",
            ),
            (Some(json!({"role": "", "name": "Send"})), "", "Send"),
            (None, "", "Send"),
        ] {
            match holder {
                Some(h) => t.eval("holder", h),
                None => std::fs::remove_file(t.dir.join("eval-holder.json")).unwrap(),
            }
            let waiting = t.spawn(&["click", "button.send span"]);
            let (req, mut conn) = asked(&gate);
            let b = &req["browser"];
            assert_eq!(
                (b["role"].as_str(), b["name"].as_str()),
                (Some(role), Some(name)),
                "{req}"
            );
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec);
        }
        t.eval("holder", json!({"role": "link", "name": "Home"}));
        assert_eq!(t.run(&["click", "a.home span"]), Next::Exec);
        assert!(gate.try_recv().is_err());
    }

    /// R54: agent-browser 0.41 rebuilds its ref map at every snapshot, and a scoped one leaves
    /// out a frame's refs, or all of them when its selector matches nothing in the page; so the
    /// wrapper's last snapshot is always a full one, which gives every element the ref it had.
    #[test]
    fn the_wrappers_reads_leave_the_agents_refs_resolving() {
        let t = T::new("bg-refs");
        t.page(MAIL, COMPOSE, "");
        t.scoped(&[("button", "Send")]);
        t.eval("forms", json!({"forms": []}));
        let gate = t.gate();
        let last_is_full = |args: &[&str]| {
            let snaps = t.snapshots();
            assert!(
                snaps.iter().any(|l| l.contains(" -s ")),
                "{args:?}: {snaps:?}"
            );
            let last = snaps.last().unwrap();
            assert!(!last.contains(" -s "), "{args:?}: {snaps:?}");
        };
        for args in [
            &["click", "#send"][..],
            &["find", "testid", "send"],
            &["find", "first", "button"],
            &["type", "#msg", "hi\n"],
        ] {
            let waiting = t.spawn(args);
            let (_, mut conn) = asked(&gate);
            last_is_full(args);
            answer(&mut conn, json!({"verdict": "allow"}));
            assert_eq!(waiting.join().unwrap(), Next::Exec, "{args:?}");
            last_is_full(args);
        }
        // A scoped read that fails rebuilds them too.
        let failed =
            r#"{"success":false,"data":null,"error":"Selector '#x' did not match any element"}"#;
        std::fs::write(t.dir.join("scoped.json"), failed).unwrap();
        let waiting = t.spawn(&["click", "#x"]);
        let (req, mut conn) = asked(&gate);
        assert_eq!(req["browser"]["role"], "", "{req}");
        last_is_full(&["click", "#x"]);
        answer(&mut conn, json!({"verdict": "hold", "text": "held"}));
        waiting.join().unwrap();
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
            // I5: a space, a no-break space, a narrow or thin one, or an apostrophe groups
            // thousands, before exactly three digits.
            ("- StaticText \"Total 1 234,50 €\"", amount(1234.5, "EUR")),
            ("- StaticText \"Total 1\u{a0}234,50\u{a0}€\"", amount(1234.5, "EUR")),
            ("- StaticText \"Total 1\u{202f}234,50\u{202f}€\"", amount(1234.5, "EUR")),
            ("- StaticText \"Total\"\n- StaticText \"12\u{2009}345\u{2009}678 ₩\"", amount(12_345_678.0, "KRW")),
            ("- StaticText \"Total CHF 1'234.50\"", amount(1234.5, "CHF")),
            ("- StaticText \"Total CHF 1’234.50\"", amount(1234.5, "CHF")),
            ("- StaticText \"Total\"\n- StaticText \"$ 1 054.20\"", amount(1054.2, "USD")),
            // A total that cannot be read one way only is no total: over any cap.
            ("- StaticText \"Total 1 23,50 €\"", None),
            ("- StaticText \"Total 12 3456 €\"", None),
            ("- StaticText \"Total 1.23.45 €\"", None),
            ("- StaticText \"Total 1,234,56 €\"", None),
            ("- StaticText \"Total 1 234.567,89 €\"", None),
            ("- StaticText \"Total ₹1,23,456\"", None),
            ("- StaticText \"Total\"\n- StaticText \"1'23 CHF\"", None),
            ("- StaticText \"Subtotal $50\"", None),
            ("- button \"Pay\" [ref=e1]", None),
        ] {
            assert_eq!(page_total(tree), want, "{tree}");
        }
    }

    /// The page scripts parse as JavaScript (checked with `node --check` where node is).
    #[test]
    fn the_page_scripts_are_javascript() {
        let sel = format!("const sel = {};", Value::from("a[title=\"it's\"] > span"));
        let forms = |sel: Value| format!("const sel = {sel}, name = {};", Value::from("Q's"));
        let scripts = [
            script("focus", "", FOCUS_BODY),
            script("holder", &sel, HOLDER_BODY),
            script("forms", &forms(Value::Null), FORMS_BODY),
            script("forms", &forms(Value::from("#q")), FORMS_BODY),
            script("submit", "", SUBMIT_BODY),
        ];
        let dir = crate::test_dir("bg-scripts");
        for (i, s) in scripts.iter().enumerate() {
            assert!(s.starts_with("/*zephyr:"), "{s}");
            let path = dir.join(format!("{i}.js"));
            std::fs::write(&path, s).unwrap();
            match Command::new("node").arg("--check").arg(&path).output() {
                Ok(out) => assert!(
                    out.status.success(),
                    "{s}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                ),
                Err(_) => eprintln!("skipped: no node to check the page scripts with"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
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
