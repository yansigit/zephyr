//! The approvals gate's decision and its card (approvals spec §4.2–4.3, §4.7). Every message,
//! post or payment the agent makes comes through `decide`, which alone reads the owner's modes,
//! the remembered targets and the private facts; the router (main's `exec`) and the gate socket
//! (tasks, jobs, connectors and the browser wrapper) only carry requests to it and verdicts back.
//!
//! The card and every notice are the daemon's words, built from the call itself. The agent's
//! reason shows only as one quoted line, and whatever came from the agent or a page is put on a
//! labelled line of its own, quoted, with the vault's and the facts' values scrubbed out first.

use std::sync::Mutex;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use zephyr_policy::policy::{self, Amount, Class, Gated, UNKNOWN};

use crate::approvals::{self, Effective, FactsMode, Mode};
use crate::config::Config;
use crate::facts::{self, Fact};
use crate::settings::{quoted, quoted_cut};
use crate::store::{Action, ActionKind, NewAction, Origin, Store};

/// At most this many actions wait from one run (main's run, a task, a job run) ...
pub const RUN_CAP: u32 = 3;
/// ... and at most this many are held in a day.
pub const DAY_CAP: u32 = 20;
/// The longest a browser wait holds the page, in minutes; a longer delay is cut to it.
pub const WAIT_MINUTES: u32 = 10;
/// The class of a call held only for a private fact: one outside messages, posts and money.
pub const OUTBOUND: &str = "outbound";
/// What a call whose private facts could not be read is held as.
pub const UNREADABLE: &str = "your private facts could not be read";
/// Why values are not shown when a fact is split across them.
const SPLIT: &str = "it holds a private fact split across its values";
/// The most of a call's arguments an audit row keeps, in bytes.
const AUDIT_ARGS: usize = 500;
/// The longest command or arguments a card shows.
const BODY_CHARS: usize = 1500;
/// The longest subject, first lines, page, button or typed text a card shows.
const LINE_CHARS: usize = 200;

/// One call for the gate to decide: main's `exec`, a task's or job's `bash`, any `mcp_call`, or
/// what the browser wrapper caught (`tool` "browser", `browser` set). `run` is the run key
/// (`Origin::run`): the session id for tasks and jobs, `main:<run start ms>` for main. `reason`
/// is the run's latest assistant text, which a card quotes and nothing else uses.
#[derive(Clone)]
pub struct Request {
    pub session: String,
    pub call_id: Option<String>,
    pub tool: String,
    pub args: Value,
    pub browser: Option<Browser>,
    pub run: String,
    pub reason: Option<String>,
}

/// What the browser wrapper read about the page and the control (approvals spec §4.4):
/// `action` is the wrapper's command (`click`, `press`, `fill`, ...), `typed` the values typed
/// into the page (checked for private facts), the flags what `browser_class` takes. It reads
/// the control's `role` and `name` flat or as the spec's `element: {role, name}`; a control
/// whose role is missing either way is one the gate could not read, and is held.
#[derive(Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "BrowserWire")]
pub struct Browser {
    pub action: String,
    pub url: String,
    pub origin: String,
    pub role: String,
    pub name: String,
    pub typed: Vec<String>,
    pub form_textarea: bool,
    pub form_password: bool,
    pub composer: bool,
}

/// `Browser` as it arrives: the control flat (`role`, `name`) or nested (`element`).
#[derive(Default, Deserialize)]
#[serde(default)]
struct BrowserWire {
    action: String,
    url: String,
    origin: String,
    role: String,
    name: String,
    element: Element,
    typed: Vec<String>,
    form_textarea: bool,
    form_password: bool,
    composer: bool,
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Element {
    role: String,
    name: String,
}

impl From<BrowserWire> for Browser {
    fn from(w: BrowserWire) -> Browser {
        let pick = |flat: String, nested: String| match flat.trim().is_empty() {
            true => nested,
            false => flat,
        };
        Browser {
            action: w.action,
            url: w.url,
            origin: w.origin,
            role: pick(w.role, w.element.role),
            name: pick(w.name, w.element.name),
            typed: w.typed,
            form_textarea: w.form_textarea,
            form_password: w.form_password,
            composer: w.composer,
        }
    }
}

/// Lengths only for what was typed: it can hold a private fact.
impl std::fmt::Debug for Browser {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let typed: Vec<usize> = self.typed.iter().map(String::len).collect();
        f.debug_struct("Browser")
            .field("action", &self.action)
            .field("origin", &self.origin)
            .field("role", &self.role)
            .field("typed", &typed)
            .finish_non_exhaustive()
    }
}

/// What the call does now. `Hold` answers the call with `text` (`held_text`); `Wait` keeps a
/// browser action waiting in place for the owner (or out its delay); `Deny` refuses it at once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Hold { id: i64, text: String },
    Wait { id: i64 },
    Deny(String),
}

/// What the agent is told of a held action (approvals spec §4.8).
pub fn held_text(id: i64, summary: &str) -> String {
    format!("held for the owner as #{id} ({summary}); don't retry it; carry on or end your turn")
}

/// Decides `req` (approvals spec §4.2): a call that is no message, post or payment and sends
/// nothing out is allowed at once. Otherwise, in order:
///
/// 1. a connector call the owner approved goes through, once (its pass, §4.6);
/// 2. the private facts: found with `hold`, or unreadable unless `off`, hold it whatever the
///    class's mode; found with `warn`, the owner is told once it runs;
/// 3. the class's mode: `ask`, `ask-new` (known targets go), `delay N`, `auto` (money with a cap:
///    a known amount, in `approvals.currency`, at most the cap);
/// 4. the flood cap, before anything would wait: 3 actions waiting from one run, 20 held a day;
///    past it the call is denied, and the owner gets one summary line per run.
///
/// A held action's card goes to `origin` (channel, sender). Every decision on a call that sends
/// is an audit row. A store error denies the call: the gate fails closed.
pub fn decide(
    cfg: &Config,
    store: &Store,
    req: &Request,
    origin: (String, String),
    now: i64,
) -> Verdict {
    // What the wrapper sends always carries the page: without it, nothing could be checked.
    if req.tool == "browser" && req.browser.is_none() {
        let scrub = Scrub::load(cfg);
        audit_row(
            store,
            &req.session,
            &req.tool,
            &scrub.json(&req.args),
            "denied",
        );
        return Verdict::Deny("not run: the gate could not read the browser request".into());
    }
    let Some(call) = Call::of(cfg, req) else {
        return Verdict::Allow;
    };
    let scrub = Scrub::load(cfg);
    let gate = Gate {
        cfg,
        store,
        req,
        call: &call,
        scrub: &scrub,
        origin: &origin,
        now,
    };
    gate.judge().unwrap_or_else(|e| {
        eprintln!("zephyr: the gate could not decide a {} call: {e}", req.tool);
        gate.audit("denied");
        Verdict::Deny("not run: the gate could not read its records".into())
    })
}

/// The call as the gate sees it.
struct Call {
    gated: Option<Gated>,
    kind: ActionKind,
    /// `messages`, `posts`, `money`, or [`OUTBOUND`] for a call held only for a fact.
    class: String,
    /// What the private facts are looked for in.
    text: String,
    /// Where the call goes, which a fact's `allow` list is compared with; `ask-new`'s targets.
    targets: Vec<String>,
    /// One line in the daemon's words.
    summary: String,
    /// What runs on `/ok`: the exact command, the `mcp_call` arguments, or the browser request.
    payload: String,
    secrets: Vec<String>,
    /// What the audit row records, scrubbed value by value when it is written.
    audit: Value,
    /// A browser action on a control the wrapper could not read (no role): held whatever the
    /// modes say (spec §4.4).
    unread: bool,
}

impl Call {
    /// `None` for a call the gate has nothing to look at: no message, post or payment, and no
    /// data going out (spec §4.2 step 1: the facts are checked in every call `outbound` lists,
    /// every connector call, and the browser's typed values).
    fn of(cfg: &Config, req: &Request) -> Option<Call> {
        if let Some(b) = &req.browser {
            return Call::browser(req, b);
        }
        let classes = |server: &str, tool: &str| connector_class(cfg, server, tool);
        let gated = policy::gated(&req.tool, &req.args, &classes);
        let (kind, text, targets, summary) = match req.tool.as_str() {
            "bash" | "exec" => {
                let command = req.args.get("command")?.as_str()?;
                let out = policy::outbound(command);
                if gated.is_none() && out.is_empty() {
                    return None;
                }
                (ActionKind::Run, command.to_string(), vec![], out.join("; "))
            }
            "mcp_call" => {
                let field = |k: &str| req.args.get(k).and_then(Value::as_str).unwrap_or("");
                let label = format!("{}/{}", field("server").to_lowercase(), field("tool"));
                // The values the call carries, decoded (C2): never its JSON text, where a
                // newline is `\n` and no fact written across two lines would match.
                (
                    ActionKind::Pass,
                    decoded(&req.args),
                    vec![label.clone()],
                    label,
                )
            }
            _ => return None,
        };
        let (class, targets, summary) = match &gated {
            Some(g) => (class_key(g.class), g.targets.clone(), g.summary.clone()),
            None => (OUTBOUND, targets, summary),
        };
        let secrets = (req.args.get("secrets").and_then(Value::as_array))
            .map(|names| {
                let names = names.iter().filter_map(Value::as_str);
                names.map(String::from).collect()
            })
            .unwrap_or_default();
        let payload = match kind {
            ActionKind::Run => text.clone(),
            _ => req.args.to_string(),
        };
        Some(Call {
            gated,
            kind,
            class: class.into(),
            payload,
            text,
            targets,
            summary,
            secrets,
            audit: req.args.clone(),
            unread: false,
        })
    }

    /// A click or key press the wrapper caught, classified by `browser_class`, or typing, which
    /// only the facts check looks at. Its target is the page's origin. Any action but typing on
    /// a control with no role is one the gate could not read: held, never let through (I3).
    fn browser(req: &Request, b: &Browser) -> Option<Call> {
        let origin = origin_of(&b.origin);
        let typing = matches!(b.action.as_str(), "fill" | "type");
        let unread = !typing && b.role.trim().is_empty();
        // R39: with no role, a readable name still gives its class (`browser_class` reads an
        // empty role as a button's); the action is held either way.
        let class = match typing {
            true => None,
            false => {
                let (role, name) = (&b.role, &b.name);
                policy::browser_class(role, name, b.form_textarea, b.form_password, b.composer)
            }
        };
        let text = b.typed.join("\n");
        if class.is_none() && !unread && text.trim().is_empty() {
            return None;
        }
        let name = quoted_cut(&b.name, 80);
        let summary = match b.action.as_str() {
            _ if typing => format!("typing on {origin}"),
            _ if unread && name.is_empty() => {
                format!("a click the gate could not read on {origin}")
            }
            "press" | "keyboard" if name.is_empty() => format!("Enter on {origin}"),
            "press" | "keyboard" => format!("Enter in \"{name}\" on {origin}"),
            _ => format!("click \"{name}\" on {origin}"),
        };
        let audit = serde_json::json!({ "args": req.args, "browser": b });
        Some(Call {
            gated: class.map(|class| Gated {
                class,
                targets: vec![origin.clone()],
                summary: summary.clone(),
                amount: None,
                replay: policy::Replay::Wait,
            }),
            kind: ActionKind::Wait,
            class: class.map_or(OUTBOUND, class_key).into(),
            text,
            targets: vec![origin],
            summary,
            payload: audit.to_string(),
            secrets: Vec::new(),
            audit,
            unread,
        })
    }
}

/// Every string and number in `v`, decoded, one per line, then every key: what the facts are
/// looked for in (C2). The newline join also catches a value split across two of them, and the
/// keys come after all the values, so `{"area": "555", "number": "010-2345"}` reads
/// `555\n010-2345`, one number.
fn decoded(v: &Value) -> String {
    let (mut values, mut keys) = (Vec::new(), Vec::new());
    leaves(v, &mut values, &mut keys);
    values.append(&mut keys);
    values.join("\n")
}

/// The values of `v` alone, keys left out, one per line: the join a split fact is looked for in.
fn values_of(v: &Value) -> String {
    let (mut values, mut keys) = (Vec::new(), Vec::new());
    leaves(v, &mut values, &mut keys);
    values.join("\n")
}

fn leaves(v: &Value, values: &mut Vec<String>, keys: &mut Vec<String>) {
    match v {
        Value::String(s) => values.push(s.clone()),
        Value::Number(n) => values.push(n.to_string()),
        Value::Array(items) => items.iter().for_each(|i| leaves(i, values, keys)),
        Value::Object(map) => {
            for (k, v) in map {
                keys.push(k.clone());
                leaves(v, values, keys);
            }
        }
        Value::Bool(_) | Value::Null => {}
    }
}

/// Agent or page text as part of one card or notice line, not between quotes: control and bidi
/// characters dropped, whitespace folded, cut (C1; `settings::one_line`). Text between quotes
/// goes through `quoted_cut`, which escapes its quotes too (R41).
fn one_line(text: &str) -> String {
    crate::settings::one_line(text, LINE_CHARS)
}

/// The vault's and the private facts' values, loaded once for one decision or one card, and
/// scrubbed out of what is shown or recorded. `text` is `vault::scrubbed` followed by
/// `facts::scrub`, and fails closed as they do: nothing of the text is shown when the vault or
/// the facts cannot be read.
struct Scrub {
    secrets: Option<Vec<(String, String)>>,
    facts: anyhow::Result<Vec<Fact>>,
}

impl Scrub {
    fn load(cfg: &Config) -> Scrub {
        let vault = crate::vault::Vault::load(&crate::vault::path(cfg));
        Scrub {
            secrets: vault.ok().map(|v| v.secrets()),
            facts: facts::load(&facts::path(cfg)),
        }
    }

    fn text(&self, text: &str) -> String {
        let Some(secrets) = &self.secrets else {
            return "[output withheld: the vault could not be read]".into();
        };
        let text = crate::vault::scrub(text, secrets);
        match &self.facts {
            Ok(f) => facts::scrub(f, &text),
            Err(_) => format!("[withheld: {UNREADABLE}]"),
        }
    }

    /// `v` with each string, number and key scrubbed as decoded text (C2).
    fn value(&self, v: &Value) -> Value {
        match v {
            Value::String(s) => Value::String(self.text(s)),
            Value::Number(n) => {
                let (raw, clean) = (n.to_string(), self.text(&n.to_string()));
                match raw == clean {
                    true => v.clone(),
                    false => Value::String(clean),
                }
            }
            Value::Array(items) => Value::Array(items.iter().map(|i| self.value(i)).collect()),
            Value::Object(map) => {
                let map = map.iter().map(|(k, v)| (self.text(k), self.value(v)));
                Value::Object(map.collect())
            }
            Value::Bool(_) | Value::Null => v.clone(),
        }
    }

    /// `v` scrubbed and written as JSON. A fact split across values, each part too short to
    /// match alone, still matches their join (values only, keys left out): then none of it is
    /// shown.
    fn json(&self, v: &Value) -> String {
        let clean = self.value(v);
        match self.split(&values_of(&clean)) {
            true => format!("[withheld: {SPLIT}]"),
            false => clean.to_string(),
        }
    }

    /// Whether `joined`, values already scrubbed one by one, still holds a fact: one split
    /// across them.
    fn split(&self, joined: &str) -> bool {
        self.facts
            .as_ref()
            .is_ok_and(|f| !names_in(f, joined, &[]).is_empty())
    }
}

/// One audit row, its arguments cut to 500 bytes.
fn audit_row(store: &Store, session: &str, tool: &str, args: &str, outcome: &str) {
    let end = (0..=AUDIT_ARGS.min(args.len()))
        .rev()
        .find(|&i| args.is_char_boundary(i))
        .unwrap_or(0);
    if let Err(e) = store.audit(session, tool, &args[..end], outcome) {
        eprintln!("zephyr: writing the audit failed: {e}");
    }
}

/// A page origin as a target: lower case, no trailing slash; `unknown` when the wrapper had none.
fn origin_of(raw: &str) -> String {
    let o = raw.trim().trim_end_matches('/').to_lowercase();
    if o.is_empty() {
        UNKNOWN.into()
    } else {
        o
    }
}

/// The owner's `classes` entry for a connector's tool (spec §4.1): `Some(None)` for `none`. A
/// value validation would refuse counts as the class whose mode is strictest now, never `none`.
fn connector_class(cfg: &Config, server: &str, tool: &str) -> Option<Option<Class>> {
    let mcp = cfg
        .mcp
        .iter()
        .find(|m| m.name.eq_ignore_ascii_case(server))?;
    Some(match mcp.classes.get(tool)?.trim() {
        "messages" => Some(Class::Messages),
        "posts" => Some(Class::Posts),
        "money" => Some(Class::Money),
        "none" => None,
        _ => {
            let e = approvals::effective(cfg);
            let all = [Class::Messages, Class::Posts, Class::Money];
            all.into_iter().reduce(|a, b| {
                match approvals::stricter(mode_of(&e, b), mode_of(&e, a)) {
                    true => b,
                    false => a,
                }
            })
        }
    })
}

fn mode_of(e: &Effective, class: Class) -> Mode {
    match class {
        Class::Messages => e.messages,
        Class::Posts => e.posts,
        Class::Money => e.money,
    }
}

fn class_key(class: Class) -> &'static str {
    approvals::class_name(class)
}

fn class_of(key: &str) -> Option<Class> {
    match key {
        "messages" => Some(Class::Messages),
        "posts" => Some(Class::Posts),
        "money" => Some(Class::Money),
        _ => None,
    }
}

/// What the facts check found.
enum Found {
    Nothing,
    Names(Vec<String>),
    /// The facts file is there and cannot be read.
    Unreadable,
}

/// What a decision comes to before the flood cap.
enum Plan {
    /// Run now; the audit outcome (`auto`, `known`), `None` for a call that is not gated.
    Allow(Option<&'static str>),
    /// Wait for the owner; the audit outcome (`held`, `fact-hold`).
    Hold(&'static str),
    /// Run after this many minutes unless cancelled.
    Delay(u32),
}

/// The names of the facts in `text`, once each. R38: a fact is skipped only for a call with
/// recipients every one of which its `allow` list names; `unknown` is never in it.
fn names_in(facts: &[Fact], text: &str, recipients: &[String]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    let mut add = |found: Vec<&str>| {
        for n in found {
            if !names.iter().any(|m| m == n) {
                names.push(n.into());
            }
        }
    };
    match recipients {
        [] => add(facts::found(facts, text, None)),
        all => all.iter().for_each(|t| {
            let target = (t != UNKNOWN).then_some(t.as_str());
            add(facts::found(facts, text, target))
        }),
    }
    names
}

/// The targets of a call that are where it goes, for a fact's `allow` list (R38): a connector
/// call's first target is its `server/tool`, the medium, not a recipient.
fn recipients(kind: ActionKind, targets: &[String]) -> &[String] {
    match kind {
        ActionKind::Pass => targets.get(1..).unwrap_or_default(),
        _ => targets,
    }
}

/// `contains your private fact phone`, or `facts a, b`.
fn fact_note(names: &[String]) -> String {
    let s = if names.len() > 1 { "s" } else { "" };
    format!("contains your private fact{s} {}", names.join(", "))
}

/// Whether `amount` is known, in `currency` and at most `cap` cents. R21: an amount in another
/// currency, one naming no currency, or none at all counts as over the cap. R36: the amount
/// counts rounded up to the cent, so 50.004 is over a cap of 50 (the float error of `x * 100`,
/// such as 0.1 being 10.000000000000002 cents, is rounded away first).
fn within(amount: Option<&Amount>, cap: u64, currency: &str) -> bool {
    amount.is_some_and(|a| {
        let cents = ((a.value * 100.0 * 1e6).round() / 1e6).ceil();
        a.currency == currency && cents.is_finite() && cents >= 0.0 && cents <= cap as f64
    })
}

/// A notice for the owner on `origin`, else on the channel last answered.
fn tell(store: &Store, origin: &(String, String), text: &str) {
    let (channel, to) = origin;
    let told = match channel.is_empty() {
        true => store.notify_last_owner(text),
        false => store.notify(channel, to, text),
    };
    if let Err(e) = told {
        eprintln!("zephyr: telling the owner about a held action failed: {e}");
    }
}

/// The runs whose flood the owner was told of: one summary line per run, not one per call. In
/// memory: a restart tells once more at worst.
static FLOODED: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn first_flood(run: &str) -> bool {
    let mut told = FLOODED.lock().unwrap_or_else(|e| e.into_inner());
    if told.iter().any(|r| r == run) {
        return false;
    }
    if told.len() >= 256 {
        told.remove(0);
    }
    told.push(run.to_string());
    true
}

/// One decision in the making.
struct Gate<'a> {
    cfg: &'a Config,
    store: &'a Store,
    req: &'a Request,
    call: &'a Call,
    scrub: &'a Scrub,
    origin: &'a (String, String),
    now: i64,
}

impl Gate<'_> {
    /// Scrubbed and on one line: how anything from the call goes into a notice or the summary.
    fn shown(&self, text: &str) -> String {
        one_line(&self.scrub.text(text))
    }

    /// The audit row for this call: its arguments scrubbed value by value (a command's text
    /// with its own newlines), cut to 500 bytes.
    fn audit(&self, outcome: &str) {
        let args = self.scrub.json(&self.call.audit);
        audit_row(
            self.store,
            &self.req.session,
            &self.req.tool,
            &args,
            outcome,
        );
    }

    fn judge(&self) -> rusqlite::Result<Verdict> {
        let (call, store) = (self.call, self.store);
        let e = approvals::effective(self.cfg);
        // Exactly the connector call the owner approved, once. Before the facts: the owner saw
        // the card that named them.
        if call.kind == ActionKind::Pass {
            if let Some(digest) = pass_digest(&self.req.args) {
                if store.use_pass(&digest, self.now)? {
                    self.audit("pass");
                    return Ok(Verdict::Allow);
                }
            }
        }
        let found = match (e.facts, &self.scrub.facts) {
            (FactsMode::Off, _) => Found::Nothing,
            (_, Err(_)) => Found::Unreadable,
            (_, Ok(f)) => match names_in(f, &call.text, recipients(call.kind, &call.targets)) {
                names if names.is_empty() => Found::Nothing,
                names => Found::Names(names),
            },
        };
        let plan = match (&found, &call.gated) {
            (Found::Unreadable, _) => Plan::Hold("fact-hold"),
            (Found::Names(_), _) if e.facts == FactsMode::Hold => Plan::Hold("fact-hold"),
            _ if call.unread => Plan::Hold("held"),
            (_, None) => Plan::Allow(None),
            (_, Some(g)) => self.mode(&e, g)?,
        };
        match plan {
            Plan::Allow(outcome) => {
                if let Found::Names(names) = &found {
                    let to = match call.targets.as_slice() {
                        [] => self.shown(&call.summary),
                        all => {
                            let all: Vec<String> = all.iter().map(|t| self.shown(t)).collect();
                            all.join(", ")
                        }
                    };
                    for name in names {
                        let text =
                            format!("[zephyr] the agent sent your private fact {name} to {to}");
                        tell(store, self.origin, &text);
                    }
                    self.audit("fact-warn");
                } else if let Some(outcome) = outcome {
                    self.audit(outcome);
                }
                Ok(Verdict::Allow)
            }
            Plan::Hold(outcome) => self.hold(&found, None, outcome),
            Plan::Delay(minutes) => self.hold(&found, Some(minutes), "held"),
        }
    }

    /// What the class's mode does with a gated call (spec §4.3).
    fn mode(&self, e: &Effective, g: &Gated) -> rusqlite::Result<Plan> {
        let class = class_key(g.class);
        Ok(match mode_of(e, g.class) {
            Mode::Ask => Plan::Hold("held"),
            Mode::AskNew if self.store.all_known(class, &g.targets)? => Plan::Allow(Some("known")),
            Mode::AskNew => Plan::Hold("held"),
            Mode::Delay(minutes) => Plan::Delay(minutes),
            Mode::Auto => Plan::Allow(Some("auto")),
            Mode::AutoCap(cap)
                if within(g.amount.as_ref(), cap, &approvals::currency(self.cfg)) =>
            {
                Plan::Allow(Some("auto"))
            }
            Mode::AutoCap(_) => Plan::Hold("held"),
        })
    }

    /// The flood cap: the deny text and the owner's summary line, when this run or this day
    /// already has as many waiting as it may.
    fn flooded(&self) -> rusqlite::Result<Option<(String, String)>> {
        let deny = |n: u32| format!("not run: {n} actions already wait for the owner");
        let waiting = self.store.waiting_in_run(&self.req.run)?;
        if waiting >= RUN_CAP {
            return Ok(Some((
                deny(waiting),
                format!(
                    "[zephyr] {waiting} actions from one run already wait for you, so its further \
                     messages, posts and payments are not run; /held lists the waiting ones"
                ),
            )));
        }
        let today = self.store.held_today(self.now)?;
        Ok((today >= DAY_CAP).then(|| {
            (
                deny(today),
                format!(
                    "[zephyr] {today} actions were held for you in the last 24 h, so the agent's \
                     further messages, posts and payments are not run for now; /held lists the \
                     waiting ones"
                ),
            )
        }))
    }

    /// Holds the call for the owner (or, with `delay`, for that many minutes), and sends the
    /// card to the run's channel. A browser action waits in place.
    fn hold(&self, found: &Found, delay: Option<u32>, outcome: &str) -> rusqlite::Result<Verdict> {
        let (call, req) = (self.call, self.req);
        if let Some((deny, notice)) = self.flooded()? {
            if first_flood(&req.run) {
                tell(self.store, self.origin, &notice);
            }
            self.audit("denied");
            return Ok(Verdict::Deny(deny));
        }
        let wait = call.kind == ActionKind::Wait;
        let due_at = delay.map(|n| {
            let n = if wait { n.min(WAIT_MINUTES) } else { n };
            self.now + 60 * i64::from(n)
        });
        // On one line (C1): `/held` and the agent's held text print it as it is. The fact note
        // is the daemon's, with names that are only `[a-z0-9_-]`.
        let summary = match found {
            Found::Unreadable => UNREADABLE.to_string(),
            Found::Names(names) => format!("{}; {}", self.shown(&call.summary), fact_note(names)),
            Found::Nothing => self.shown(&call.summary),
        };
        let reason = (req.reason.as_deref())
            // One line and cut, not yet escaped: the card quotes it (once) when it shows it.
            .map(|r| one_line(&self.scrub.text(r)))
            .filter(|r| !r.is_empty());
        let new = NewAction {
            kind: call.kind,
            class: call.class.clone(),
            targets: call.targets.clone(),
            summary,
            reason,
            payload: call.payload.clone(),
            secrets: call.secrets.clone(),
            origin: Origin {
                channel: self.origin.0.clone(),
                sender: self.origin.1.clone(),
                session: req.session.clone(),
                run: req.run.clone(),
                call_id: req.call_id.clone(),
            },
            due_at,
        };
        let id = self.store.hold_action(&new)?;
        let action = Action {
            id,
            kind: new.kind,
            class: new.class,
            targets: new.targets,
            summary: new.summary,
            reason: new.reason,
            payload: new.payload,
            secrets: new.secrets,
            origin: new.origin,
            status: if due_at.is_some() {
                "scheduled"
            } else {
                "pending"
            }
            .into(),
            due_at,
            created_at: self.now,
        };
        tell(
            self.store,
            self.origin,
            &card(&action, self.cfg, self.store),
        );
        self.audit(outcome);
        Ok(match wait {
            true => Verdict::Wait { id },
            false => Verdict::Hold {
                id,
                text: held_text(id, &action.summary),
            },
        })
    }
}

/// The one-time pass of an approved connector call (spec §4.6): SHA-256 of its server, tool and
/// arguments with their keys sorted. `None` when the call names no server or tool.
pub fn pass_digest(args: &Value) -> Option<String> {
    let server = args.get("server")?.as_str()?;
    let tool = args.get("tool")?.as_str()?;
    let arguments = canonical(args.get("arguments").unwrap_or(&Value::Null));
    let mut ctx = ring::digest::Context::new(&ring::digest::SHA256);
    for part in [server.as_bytes(), tool.as_bytes(), arguments.as_bytes()] {
        ctx.update(&(part.len() as u64).to_be_bytes());
        ctx.update(part);
    }
    Some(data_encoding::HEXLOWER.encode(ctx.finish().as_ref()))
}

/// `v` as JSON with every object's keys in order.
fn canonical(v: &Value) -> String {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            let items: Vec<String> = keys
                .into_iter()
                .map(|k| format!("{}:{}", Value::String(k.clone()), canonical(&map[k])))
                .collect();
            format!("{{{}}}", items.join(","))
        }
        Value::Array(items) => {
            let items: Vec<String> = items.iter().map(canonical).collect();
            format!("[{}]", items.join(","))
        }
        other => other.to_string(),
    }
}

// ---- the card ----

/// The card for held action `a` (spec §4.7), as the owner reads it on the channel that asked,
/// in `/held` and in the TUI's Waiting tab:
///
/// ```text
/// [zephyr] approve #7: message (email)
/// to: alice@example.com · subject: "Invoice March"
/// first lines: "Hi Alice, attached is the invoice for…"
/// command: "himalaya message send <<'EOF' To: alice@example.com …"
/// the agent says: "the owner asked me to send the March invoice"
/// /ok 7 · /ok 7 once · /no 7 · expires in 24 h
/// ```
///
/// Every line but the first and the last starts with a label of the daemon's; what came from
/// the agent or a page is quoted on one line and cut, with the vault's and the facts' values
/// scrubbed out (none of it is shown when the facts cannot be read). A fact hold names the fact,
/// never its value; under `ask-new` a target not approved before is marked `(new)`; a delayed
/// action says when it goes; one that needs a strong channel to approve says so.
pub fn card(a: &Action, cfg: &Config, store: &Store) -> String {
    let scrub = Scrub::load(cfg);
    let clean = |text: &str| scrub.text(text);
    let e = approvals::effective(cfg);
    let class = class_of(&a.class);
    let ask_new = class.is_some_and(|c| mode_of(&e, c) == Mode::AskNew);
    // Each target scrubbed and on one line (C1): a server, a recipient or an origin is the
    // agent's or a page's text.
    let targets: Vec<String> = (a.targets.iter())
        .map(|t| {
            let known = || store.all_known(&a.class, std::slice::from_ref(t));
            let new = ask_new && t != UNKNOWN && !known().unwrap_or(false);
            format!("{}{}", one_line(&clean(t)), if new { " (new)" } else { "" })
        })
        .collect();
    let verb = match a.status.as_str() {
        "scheduled" => "delayed",
        _ => "approve",
    };
    let browser = (a.kind == ActionKind::Wait)
        .then(|| browser_of(&a.payload))
        .flatten();
    let mut lines = vec![format!(
        "[zephyr] {verb} #{}: {}",
        a.id,
        what(class, a.kind, browser.as_ref())
    )];
    // A command is plain text, scrubbed whole; a connector call and a browser request are
    // decoded first and scrubbed value by value (C2).
    let command = (a.kind == ActionKind::Run).then(|| clean(&a.payload));
    lines.extend(match a.kind {
        ActionKind::Run => {
            let command = command.as_deref().unwrap_or_default();
            run_lines(cfg, a, class, &targets, command, &clean)
        }
        ActionKind::Pass => pass_lines(a, class, &targets, &scrub),
        ActionKind::Wait => wait_lines(class, &targets, browser.as_ref(), &scrub),
    });
    if e.facts != FactsMode::Off {
        match &scrub.facts {
            Err(_) => lines.push(UNREADABLE.into()),
            Ok(f) => {
                let names = names_in(f, &checked_text(a), recipients(a.kind, &a.targets));
                if !names.is_empty() {
                    lines.push(fact_note(&names));
                }
            }
        }
    }
    if let Some(command) = command {
        lines.push(format!("command: \"{}\"", quoted_cut(&command, BODY_CHARS)));
    }
    if let Some(reason) = a.reason.as_deref().map(|r| quoted(&clean(r))) {
        if !reason.is_empty() {
            lines.push(format!("the agent says: \"{reason}\""));
        }
    }
    lines.push(footer(a, cfg, class));
    lines.join("\n")
}

/// The card's first line after the id: `message (email)`, `payment (browser)`.
fn what(class: Option<Class>, kind: ActionKind, browser: Option<&Browser>) -> String {
    let noun = match class {
        Some(Class::Messages) => "message",
        Some(Class::Posts) => "post",
        Some(Class::Money) => "payment",
        None => {
            let typing = browser.is_some_and(|b| matches!(b.action.as_str(), "fill" | "type"));
            let unread = browser.is_none_or(|b| b.role.trim().is_empty());
            return match kind {
                ActionKind::Run => "outgoing command",
                ActionKind::Pass => "connector call",
                ActionKind::Wait if typing => "typing in the browser",
                ActionKind::Wait if unread => "a click the gate could not read (browser)",
                ActionKind::Wait => "click in the browser",
            }
            .into();
        }
    };
    let medium = match (kind, class) {
        (ActionKind::Run, Some(Class::Messages)) => "email",
        (ActionKind::Run, Some(Class::Posts)) => "GitHub",
        (ActionKind::Run, _) => "command",
        (ActionKind::Pass, _) => "connector",
        (ActionKind::Wait, _) => "browser",
    };
    format!("{noun} ({medium})")
}

/// What the facts check looked at in `a`: the command, the connector call's values decoded,
/// or what was typed.
fn checked_text(a: &Action) -> String {
    match a.kind {
        ActionKind::Run => a.payload.clone(),
        ActionKind::Pass => match serde_json::from_str::<Value>(&a.payload) {
            Ok(v) => decoded(&v),
            Err(_) => a.payload.clone(),
        },
        ActionKind::Wait => browser_of(&a.payload).map_or_else(String::new, |b| b.typed.join("\n")),
    }
}

fn browser_of(payload: &str) -> Option<Browser> {
    let v: Value = serde_json::from_str(payload).ok()?;
    serde_json::from_value(v.get("browser")?.clone()).ok()
}

/// A shell command's lines: who a mail goes to with its subject and first lines, a GitHub post's
/// repo and title, or what an outgoing command sends. `payload` is the command, plain text,
/// scrubbed whole.
fn run_lines(
    cfg: &Config,
    a: &Action,
    class: Option<Class>,
    targets: &[String],
    payload: &str,
    clean: &dyn Fn(&str) -> String,
) -> Vec<String> {
    let mut lines = Vec::new();
    match class {
        Some(Class::Messages) => {
            let (subject, body) = mail_parts(payload);
            let to = match a.targets.as_slice() {
                [t] if t == UNKNOWN => "recipients the gate could not read".to_string(),
                _ => targets.join(", "),
            };
            let subject = subject.map_or(String::new(), |s| {
                format!(" · subject: \"{}\"", quoted_cut(&s, LINE_CHARS))
            });
            lines.push(format!("to: {to}{subject}"));
            lines.extend(body.map(|b| format!("first lines: \"{}\"", quoted_cut(&b, LINE_CHARS))));
        }
        _ => {
            // The daemon's own label for it, read again from the command (the stored summary
            // carries the fact note the agent is told).
            let classes = |server: &str, tool: &str| connector_class(cfg, server, tool);
            let args = serde_json::json!({ "command": a.payload });
            let label = match policy::gated("bash", &args, &classes) {
                Some(g) => g.summary,
                None => policy::outbound(&a.payload).join("; "),
            };
            let (title, body) = match class {
                Some(Class::Posts) => gh_parts(payload),
                _ => (None, None),
            };
            let title = title.map_or(String::new(), |t| {
                format!(" · title: \"{}\"", quoted_cut(&t, LINE_CHARS))
            });
            let label = one_line(&clean(&label));
            if !label.is_empty() || !title.is_empty() {
                lines.push(format!("{label}{title}"));
            }
            lines.extend(body.map(|b| format!("first lines: \"{}\"", quoted_cut(&b, LINE_CHARS))));
        }
    }
    lines
}

/// A connector call's lines: the tool and, for a message or a post, whoever it goes to; the
/// amount for a payment; the arguments as sent, decoded, scrubbed value by value and written
/// again (C2). `targets` are shown already: the tool first, then the recipients (R37).
fn pass_lines(a: &Action, class: Option<Class>, targets: &[String], scrub: &Scrub) -> Vec<String> {
    let (tool, to) = targets
        .split_first()
        .map_or(("", &[][..]), |(t, to)| (t, to));
    let mut first = format!("tool: {tool}");
    if matches!(class, Some(Class::Messages | Class::Posts)) {
        let to = match a.targets.get(1..).unwrap_or_default() {
            [t] if t == UNKNOWN => "recipients the gate could not read".to_string(),
            _ => to.join(", "),
        };
        first += &format!(" · to: {to}");
    }
    let mut lines = vec![first];
    let raw: Value = serde_json::from_str(&a.payload).unwrap_or_default();
    if class == Some(Class::Money) {
        let gated = policy::gated("mcp_call", &raw, &|_, _| Some(Some(Class::Money)));
        lines.push(match gated.and_then(|g| g.amount) {
            None => "amount: could not be read".into(),
            Some(m) => format!("amount: {}", one_line(&scrub.text(&amount(&m)))),
        });
    }
    let arguments = match raw.get("arguments") {
        Some(v) => scrub.json(v),
        None if raw.is_null() => scrub.text(&a.payload),
        None => String::new(),
    };
    lines.push(format!(
        "arguments: \"{}\"",
        quoted_cut(&arguments, BODY_CHARS)
    ));
    lines
}

/// `20 EUR`, `50.004 USD`, `20, no currency named`: the value as the call gave it.
fn amount(m: &Amount) -> String {
    match m.currency.is_empty() {
        true => format!("{}, no currency named", m.value),
        false => format!("{} {}", m.value, m.currency),
    }
}

/// A browser action's lines: the origin, the page and the control, what was typed, and for a
/// payment that the amount could not be read (the wrapper reads none). `browser` is the
/// request decoded; each of its fields is scrubbed on its own (C2), and what was typed is not
/// shown at all when a fact is split across its entries.
fn wait_lines(
    class: Option<Class>,
    targets: &[String],
    browser: Option<&Browser>,
    scrub: &Scrub,
) -> Vec<String> {
    let mut lines = vec![format!("on: {}", targets.join(", "))];
    let Some(b) = browser else {
        lines.push("page: could not be read".into());
        return lines;
    };
    let page = format!("page: \"{}\"", quoted_cut(&scrub.text(&b.url), LINE_CHARS));
    let name = quoted_cut(&scrub.text(&b.name), LINE_CHARS);
    // R39: a control with no role is one the gate could not read; its name, when there is one,
    // still shows (and gave the class).
    let unread = b.role.trim().is_empty();
    lines.push(match b.action.as_str() {
        "fill" | "type" => page,
        _ if unread && name.is_empty() => format!("{page} · the control could not be read"),
        _ if unread => format!("{page} · button: \"{name}\" (its role could not be read)"),
        "press" | "keyboard" => format!("{page} · key: Enter in \"{name}\""),
        _ => format!("{page} · button: \"{name}\""),
    });
    if !b.typed.is_empty() {
        let typed: Vec<String> = b.typed.iter().map(|t| scrub.text(t)).collect();
        match scrub.split(&typed.join("\n")) {
            true => lines.push(format!("typed: [withheld: {SPLIT}]")),
            false => {
                let typed: Vec<String> = (typed.iter())
                    .map(|t| format!("\"{}\"", quoted_cut(t, LINE_CHARS)))
                    .collect();
                lines.push(format!("typed: {}", typed.join(" · ")));
            }
        }
    }
    if class == Some(Class::Money) {
        lines.push("amount: could not be read".into());
    }
    lines
}

/// The last line: how to answer and when the action expires, or when a delayed one goes.
fn footer(a: &Action, cfg: &Config, class: Option<Class>) -> String {
    let id = a.id;
    if a.status == "scheduled" {
        let tz = crate::scheduler::zone(cfg).unwrap_or(jiff::tz::TimeZone::UTC);
        let at = crate::settings::clock(a.due_at.unwrap_or(a.created_at), &tz);
        return format!("sending at {at} unless /cancel {id}");
    }
    // `once` approves without remembering the targets, which only messages and posts keep.
    let once = match class {
        Some(Class::Messages | Class::Posts) => format!(" · /ok {id} once"),
        _ => String::new(),
    };
    // What is left of the 24 h (10 min for a browser wait), rounded up.
    let now = crate::now();
    let left = match a.kind {
        ActionKind::Wait => {
            let s = (a.created_at + 60 * i64::from(WAIT_MINUTES) - now).max(60);
            format!("{} min", (s + 59) / 60)
        }
        _ => {
            let s = (a.created_at + 86_400 - now).max(3600);
            format!("{} h", (s + 3599) / 3600)
        }
    };
    let mut line = format!("/ok {id}{once} · /no {id} · expires in {left}");
    let money_strong = approvals::strong_only(cfg).iter().any(|s| s == "money");
    if class == Some(Class::Money) && money_strong {
        let strong = cfg.settings.strong_channels.join(" or ");
        line += &format!(" · this one needs {strong}");
    }
    line
}

/// The subject and the first lines of the message a send carries in its own text: a heredoc's
/// headers and body, or `-H "Subject: …"`. `None` for each that is not there.
fn mail_parts(command: &str) -> (Option<String>, Option<String>) {
    let lines: Vec<&str> = command.lines().collect();
    let header = |line: &str| -> Option<(String, String)> {
        let (name, value) = line.trim_start().split_once(':')?;
        let ok = name.starts_with(|c: char| c.is_ascii_alphabetic())
            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-');
        ok.then(|| (name.to_ascii_lowercase(), value.trim().to_string()))
    };
    let addressed = |line: &&str| {
        header(line)
            .is_some_and(|(n, _)| ["to", "from", "cc", "bcc", "subject"].contains(&n.as_str()))
    };
    let ends = delimiters(command);
    let (mut subject, mut body) = (None, None);
    if let Some(start) = lines.iter().position(addressed) {
        let mut i = start;
        while let Some((name, value)) = lines.get(i).and_then(|l| header(l)) {
            if name == "subject" && subject.is_none() && !value.is_empty() {
                subject = Some(value);
            }
            i += 1;
        }
        if lines.get(i).is_some_and(|l| l.trim().is_empty()) {
            let text: Vec<&str> = (lines[i + 1..].iter())
                .map(|l| l.trim())
                .take_while(|l| !ends.iter().any(|d| d == l))
                .filter(|l| !l.is_empty())
                .collect();
            if !text.is_empty() {
                body = Some(text.join(" "));
            }
        }
    }
    if subject.is_none() {
        let words = words(command);
        subject = (words.iter())
            .filter_map(|w| header(w))
            .find(|(n, v)| n == "subject" && !v.is_empty())
            .map(|(_, v)| v);
    }
    (subject, body)
}

/// The words heredocs in `command` end at (`<<EOF`, `<<-'EOF'`).
fn delimiters(command: &str) -> Vec<String> {
    (command.match_indices("<<"))
        .filter_map(|(i, _)| {
            let rest = command[i + 2..].trim_start_matches('-').trim_start();
            let rest = rest.trim_start_matches(['\'', '"']);
            let word: String = (rest.chars())
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            (!word.is_empty()).then_some(word)
        })
        .collect()
}

/// A GitHub post's title and body from its flags (`--title`, `--body`, `gh api -f title=…`).
fn gh_parts(command: &str) -> (Option<String>, Option<String>) {
    let w = words(command);
    let api = w.iter().any(|a| a == "api");
    let flag = |names: &[&str]| {
        w.iter().enumerate().find_map(|(i, a)| {
            names.iter().find_map(|n| match a.strip_prefix(n) {
                Some("") => w.get(i + 1).cloned(),
                Some(v) => v.strip_prefix('=').map(String::from),
                None => None,
            })
        })
    };
    let field = |key: &str| {
        let fields = ["-f", "-F", "--field", "--raw-field"];
        w.iter().enumerate().find_map(|(i, a)| {
            let value = match fields.contains(&a.as_str()) {
                true => w.get(i + 1).map(String::as_str),
                false => (a.strip_prefix("-f").or_else(|| a.strip_prefix("-F")))
                    .filter(|v| !v.starts_with('-')),
            };
            value?
                .strip_prefix(key)?
                .strip_prefix('=')
                .map(String::from)
        })
    };
    let (title, body) = match api {
        true => (field("title"), field("body")),
        false => (flag(&["--title", "-t"]), flag(&["--body", "-b"])),
    };
    let nonempty = |v: Option<String>| v.filter(|v| !v.trim().is_empty());
    (nonempty(title), nonempty(body))
}

/// `command` split into words as a shell would, near enough to read a flag's value for a card:
/// quotes and backslashes are honoured, `;`, `|`, `&` and newlines end a word.
fn words(command: &str) -> Vec<String> {
    let (mut out, mut cur, mut open) = (Vec::new(), String::new(), false);
    let mut chars = command.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                open = true;
                cur.extend(chars.by_ref().take_while(|&d| d != '\''));
            }
            '"' => {
                open = true;
                while let Some(d) = chars.next() {
                    match d {
                        '"' => break,
                        '\\' if chars.peek().is_some_and(|n| "\"\\$`".contains(*n)) => {
                            cur.extend(chars.next());
                        }
                        d => cur.push(d),
                    }
                }
            }
            '\\' => {
                open = true;
                cur.extend(chars.next().filter(|n| *n != '\n'));
            }
            c if c.is_whitespace() || matches!(c, ';' | '|' | '&') => {
                if open || !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                    open = false;
                }
            }
            c => cur.push(c),
        }
    }
    if open || !cur.is_empty() {
        out.push(cur);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{ActionKind, Origin};
    use serde_json::json;
    use std::path::{Path, PathBuf};

    struct Home {
        dir: PathBuf,
        cfg: Config,
        store: Store,
    }

    impl Drop for Home {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn config(dir: &Path, extra: &str) -> Config {
        toml::from_str(&format!(
            "rpi = '/r'\nhome = {dir:?}\ntimezone = 'UTC'\n{extra}"
        ))
        .unwrap()
    }

    fn home(name: &str, extra: &str) -> Home {
        let dir = crate::test_dir(name);
        let cfg = config(&dir, extra);
        let store = Store::open(&cfg.db_path()).unwrap();
        Home { dir, cfg, store }
    }

    fn owner() -> (String, String) {
        ("discord".into(), "owner-1".into())
    }

    const REASON: &str = "the owner asked me to send the March invoice";

    const MAIL: &str = "himalaya message send <<'EOF'\nTo: alice@example.com\n\
                        Subject: Invoice March\n\nHi Alice, attached is the invoice for March.\n\
                        Best, A.\nEOF";

    fn mail_to(to: &str, body: &str) -> String {
        format!("himalaya message send <<'EOF'\nTo: {to}\nSubject: Hello\n\n{body}\nEOF")
    }

    fn exec(command: &str, run: &str) -> Request {
        Request {
            session: "main".into(),
            call_id: Some("call-1".into()),
            tool: "exec".into(),
            args: json!({ "command": command }),
            browser: None,
            run: run.into(),
            reason: Some(REASON.into()),
        }
    }

    fn mcp(server: &str, tool: &str, arguments: Value, run: &str) -> Request {
        Request {
            session: "task-3".into(),
            call_id: Some("call-9".into()),
            tool: "mcp_call".into(),
            args: json!({ "server": server, "tool": tool, "arguments": arguments }),
            browser: None,
            run: run.into(),
            reason: None,
        }
    }

    fn click(name: &str, origin: &str, run: &str) -> Request {
        Request {
            session: "main".into(),
            call_id: None,
            tool: "browser".into(),
            args: json!({ "argv": ["click", "@e3"] }),
            browser: Some(Browser {
                action: "click".into(),
                url: format!("{origin}/compose"),
                origin: origin.into(),
                role: "button".into(),
                name: name.into(),
                ..Browser::default()
            }),
            run: run.into(),
            reason: None,
        }
    }

    fn notices(store: &Store) -> Vec<String> {
        let rows = store.pending_outbox().unwrap();
        rows.into_iter().map(|r| r.text).collect()
    }

    fn outcomes(store: &Store) -> Vec<String> {
        let rows = store.audit_rows().unwrap();
        rows.into_iter().map(|r| r.3).collect()
    }

    fn held(v: &Verdict) -> i64 {
        match v {
            Verdict::Hold { id, .. } | Verdict::Wait { id } => *id,
            v => panic!("not held: {v:?}"),
        }
    }

    fn action(store: &Store, id: i64) -> Action {
        let all = store.held_actions().unwrap();
        all.into_iter().find(|a| a.id == id).unwrap()
    }

    #[test]
    fn manual_holds_a_mail_send_with_a_daemon_worded_card() {
        let h = home("gate-manual", "");
        let now = crate::now();
        let v = decide(&h.cfg, &h.store, &exec(MAIL, "main:1"), owner(), now);
        let Verdict::Hold { id, text } = v else {
            panic!("{v:?}")
        };
        assert_eq!(
            text,
            format!(
                "held for the owner as #{id} (email to alice@example.com); don't retry it; \
                 carry on or end your turn"
            )
        );
        assert_eq!(text, held_text(id, "email to alice@example.com"));
        let a = action(&h.store, id);
        assert_eq!(
            (a.kind, a.class.as_str(), a.status.as_str(), a.due_at),
            (ActionKind::Run, "messages", "pending", None)
        );
        // Exactly what runs on `/ok`.
        assert_eq!(a.payload, MAIL);
        assert_eq!(a.targets, ["alice@example.com"]);
        assert_eq!(a.reason.as_deref(), Some(REASON));
        assert_eq!(
            a.origin,
            Origin {
                channel: "discord".into(),
                sender: "owner-1".into(),
                session: "main".into(),
                run: "main:1".into(),
                call_id: Some("call-1".into()),
            }
        );
        // The card goes to the channel of the run that asked, in the daemon's words.
        let rows = h.store.pending_outbox().unwrap();
        assert_eq!(rows.len(), 1, "{:?}", notices(&h.store));
        assert_eq!(
            (rows[0].channel.as_str(), rows[0].recipient.as_str()),
            ("discord", "owner-1")
        );
        let want = format!(
            "[zephyr] approve #{id}: message (email)\n\
             to: alice@example.com · subject: \"Invoice March\"\n\
             first lines: \"Hi Alice, attached is the invoice for March. Best, A.\"\n\
             command: \"himalaya message send <<'EOF' To: alice@example.com Subject: Invoice \
             March Hi Alice, attached is the invoice for March. Best, A. EOF\"\n\
             the agent says: \"{REASON}\"\n\
             /ok {id} · /ok {id} once · /no {id} · expires in 24 h"
        );
        assert_eq!(rows[0].text, want);
        assert_eq!(card(&a, &h.cfg, &h.store), want);
        assert_eq!(outcomes(&h.store), ["held"]);
        // What sends nothing is none of the gate's business, and leaves no audit row.
        let ls = decide(&h.cfg, &h.store, &exec("ls -la", "main:1"), owner(), now);
        assert_eq!(ls, Verdict::Allow);
        let read = Request {
            tool: "read".into(),
            args: json!({"path": "/zephyr/notes.md"}),
            ..exec("", "main:1")
        };
        assert_eq!(
            decide(&h.cfg, &h.store, &read, owner(), now),
            Verdict::Allow
        );
        assert_eq!(outcomes(&h.store), ["held"]);
        assert_eq!(h.store.held_actions().unwrap().len(), 1);
    }

    #[test]
    fn ask_new_lets_a_known_recipient_through() {
        let h = home("gate-ask-new", "[approvals]\nmessages = 'ask-new'\n");
        h.store
            .remember_targets("messages", &["alice@example.com".into()])
            .unwrap();
        let now = crate::now();
        let d = |cmd: &str| decide(&h.cfg, &h.store, &exec(cmd, "main:2"), owner(), now);
        assert_eq!(d(&mail_to("alice@example.com", "Hi.")), Verdict::Allow);
        assert_eq!(d(&mail_to("Alice@Example.com", "Hi.")), Verdict::Allow);
        assert!(notices(&h.store).is_empty());
        // One new recipient holds the whole send, and the card says which one is new.
        let both = d(&mail_to("alice@example.com, bob@example.org", "Hi."));
        let id = held(&both);
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: message (email)\n\
                 to: alice@example.com, bob@example.org (new) · subject: \"Hello\"\n"
            )),
            "{card}"
        );
        // A recipient the gate cannot read is never a known one.
        let piped = d("cat msg.eml | himalaya message send");
        let id = held(&piped);
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: message (email)\n\
                 to: recipients the gate could not read\n"
            )),
            "{card}"
        );
        // Posts keep the preset's `ask`: a known target there is no reason to let one through.
        h.store
            .remember_targets("posts", &["octo/hello".into()])
            .unwrap();
        let gh = d("gh issue comment 4 -R octo/hello --body 'thanks'");
        held(&gh);
        // R37: a connector send is known by its tool and by every recipient it names, as an
        // `/ok` of `slack/send_message` to `#general` remembered them.
        let known = ["slack/send_message".to_string(), "#general".to_string()];
        h.store.remember_targets("messages", &known).unwrap();
        let slack = |arguments: Value| {
            let req = mcp("slack", "send_message", arguments, "task-5");
            decide(&h.cfg, &h.store, &req, owner(), now)
        };
        let general = slack(json!({"channel": "#General", "text": "hi"}));
        assert_eq!(general, Verdict::Allow);
        let id = held(&slack(json!({"channel": "#random", "text": "hi"})));
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: message (connector)\n\
                 tool: slack/send_message · to: #random (new)\n"
            )),
            "{card}"
        );
        // A send naming no recipient is one the gate cannot attribute: never known.
        let id = held(&slack(json!({"text": "hi"})));
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: message (connector)\n\
                 tool: slack/send_message · to: recipients the gate could not read\n"
            )),
            "{card}"
        );
        // A copy to someone new holds a send to a known recipient (R37 amended: cc and bcc).
        let mail = [
            "mail/send_email".to_string(),
            "alice@example.com".to_string(),
        ];
        h.store.remember_targets("messages", &mail).unwrap();
        let email = |arguments: Value| {
            let req = mcp("mail", "send_email", arguments, "task-6");
            decide(&h.cfg, &h.store, &req, owner(), now)
        };
        let to = email(json!({"to": "alice@example.com", "body": "hi"}));
        assert_eq!(to, Verdict::Allow);
        held(&email(
            json!({"to": "alice@example.com", "cc": "eve@example.org", "body": "hi"}),
        ));
        assert_eq!(
            outcomes(&h.store),
            ["known", "known", "held", "held", "held", "known", "held", "held", "known", "held"]
        );
    }

    #[test]
    fn delay_schedules_and_says_when() {
        let h = home(
            "gate-delay",
            "[approvals]\nposts = 'delay 5'\nmessages = 'delay 30'\n",
        );
        let now = crate::now();
        let gh = "gh pr create -R octo/hello --title 'Fix typo' --body 'One letter was wrong.'";
        let v = decide(&h.cfg, &h.store, &exec(gh, "main:3"), owner(), now);
        let Verdict::Hold { id, text } = v else {
            panic!("{v:?}")
        };
        assert_eq!(text, held_text(id, "gh pr create on octo/hello"));
        let a = action(&h.store, id);
        assert_eq!(
            (a.class.as_str(), a.status.as_str(), a.due_at),
            ("posts", "scheduled", Some(now + 300))
        );
        let at = crate::settings::clock(now + 300, &jiff::tz::TimeZone::UTC);
        assert_eq!(
            notices(&h.store).pop().unwrap(),
            format!(
                "[zephyr] delayed #{id}: post (GitHub)\n\
                 gh pr create on octo/hello · title: \"Fix typo\"\n\
                 first lines: \"One letter was wrong.\"\n\
                 command: \"{gh}\"\n\
                 the agent says: \"{REASON}\"\n\
                 sending at {at} unless /cancel {id}"
            )
        );
        // A browser send holds the page instead, never longer than 10 minutes.
        let v = decide(
            &h.cfg,
            &h.store,
            &click("Send", "https://mail.example.com", "main:3"),
            owner(),
            now,
        );
        let Verdict::Wait { id } = v else {
            panic!("{v:?}")
        };
        let a = action(&h.store, id);
        assert_eq!(
            (a.kind, a.status.as_str(), a.due_at),
            (ActionKind::Wait, "scheduled", Some(now + 600))
        );
        assert_eq!(a.targets, ["https://mail.example.com"]);
        let at = crate::settings::clock(now + 600, &jiff::tz::TimeZone::UTC);
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!("[zephyr] delayed #{id}: message (browser)\n")),
            "{card}"
        );
        assert!(
            card.ends_with(&format!("\nsending at {at} unless /cancel {id}")),
            "{card}"
        );
        assert_eq!(outcomes(&h.store), ["held", "held"]);
    }

    #[test]
    fn the_fourth_hold_in_a_run_is_denied_once() {
        let h = home("gate-flood", "");
        let now = crate::now();
        let d = |run: &str| decide(&h.cfg, &h.store, &exec(MAIL, run), owner(), now);
        for _ in 0..3 {
            held(&d("flood:task-7"));
        }
        let three = Verdict::Deny("not run: 3 actions already wait for the owner".into());
        assert_eq!(d("flood:task-7"), three);
        assert_eq!(d("flood:task-7"), three);
        // Three cards and one summary line, not a card per call.
        let told = notices(&h.store);
        assert_eq!(told.len(), 4, "{told:#?}");
        assert!(told[..3]
            .iter()
            .all(|n| n.starts_with("[zephyr] approve #")));
        assert!(
            told[3].starts_with("[zephyr] 3 actions from one run already wait for you"),
            "{}",
            told[3]
        );
        // Another run has three of its own; twenty a day across all of them.
        for run in ["flood:a", "flood:b", "flood:c", "flood:d", "flood:e"] {
            for _ in 0..3 {
                held(&d(run));
            }
        }
        held(&d("flood:f"));
        held(&d("flood:f"));
        assert_eq!(h.store.held_today(now).unwrap(), 20);
        let twenty = Verdict::Deny("not run: 20 actions already wait for the owner".into());
        assert_eq!(d("flood:f"), twenty);
        assert_eq!(d("flood:g"), twenty);
        assert_eq!(d("flood:g"), twenty);
        let told = notices(&h.store);
        let day = told
            .iter()
            .filter(|n| n.starts_with("[zephyr] 20 actions were held for you"))
            .count();
        assert_eq!(day, 2, "one summary for each run: {told:#?}");
        let out = outcomes(&h.store);
        assert_eq!(out.iter().filter(|o| *o == "held").count(), 20);
        assert_eq!(out.iter().filter(|o| *o == "denied").count(), 5);
    }

    const PHONE: &str = "[[fact]]\nname = \"phone\"\nkind = \"digits\"\n\
                         value = \"5550102345\"\nallow = [\"carol@example.com\"]\n";

    #[test]
    fn a_fact_holds_whatever_the_class_mode() {
        let h = home(
            "gate-facts",
            "[approvals]\npreset = 'full'\nfacts = 'hold'\n",
        );
        std::fs::write(crate::facts::path(&h.cfg), PHONE).unwrap();
        let now = crate::now();
        let body = "Call me at +1 (555) 010-2345.";
        // Messages are `auto`, yet the phone number holds the send.
        let send = exec(&mail_to("alice@example.com", body), "main:5");
        let v = decide(&h.cfg, &h.store, &send, owner(), now);
        let Verdict::Hold { id, text } = v else {
            panic!("{v:?}")
        };
        assert_eq!(
            text,
            held_text(
                id,
                "email to alice@example.com; contains your private fact phone"
            )
        );
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.contains("\ncontains your private fact phone\n"),
            "{card}"
        );
        assert!(card.contains("[fact:phone]"), "{card}");
        for value in ["010-2345", "5550102345", "(555)"] {
            assert!(!card.contains(value), "{card}");
        }
        let audit = h.store.audit_rows().unwrap();
        let (session, tool, args, outcome) = audit.last().unwrap();
        assert_eq!(
            (session.as_str(), tool.as_str(), outcome.as_str()),
            ("main", "exec", "fact-hold")
        );
        assert!(
            args.contains("[fact:phone]") && !args.contains("010-2345"),
            "{args}"
        );
        // A target the fact is allowed to go to skips the check.
        let carol = exec(&mail_to("carol@example.com", body), "main:5");
        assert_eq!(
            decide(&h.cfg, &h.store, &carol, owner(), now),
            Verdict::Allow
        );
        // Not only gated calls: whatever sends data out, a connector's arguments, typing.
        let curl = exec(
            "curl -X POST https://api.example.com/v1 -d 'phone=555 010 2345'",
            "main:5",
        );
        let id = held(&decide(&h.cfg, &h.store, &curl, owner(), now));
        assert_eq!(
            action(&h.store, id).summary,
            "curl POST api.example.com; contains your private fact phone"
        );
        let crm = mcp(
            "crm",
            "update_contact",
            json!({"phone": "5550102345"}),
            "task-3",
        );
        held(&decide(&h.cfg, &h.store, &crm, owner(), now));
        let mut typing = click("", "https://forms.example.com", "main:5");
        if let Some(b) = typing.browser.as_mut() {
            b.action = "fill".into();
            b.typed = vec!["555-010-2345".into()];
        }
        let Verdict::Wait { id } = decide(&h.cfg, &h.store, &typing, owner(), now) else {
            panic!("typing a fact waits")
        };
        assert_eq!(action(&h.store, id).kind, ActionKind::Wait);
        let plain = exec("curl -X POST https://api.example.com/v1 -d 'x=1'", "main:5");
        assert_eq!(
            decide(&h.cfg, &h.store, &plain, owner(), now),
            Verdict::Allow
        );
        assert_eq!(
            outcomes(&h.store),
            ["fact-hold", "auto", "fact-hold", "fact-hold", "fact-hold"]
        );
        // `warn` lets it go and tells the owner which fact went where.
        let warn = config(&h.dir, "[approvals]\npreset = 'full'\nfacts = 'warn'\n");
        let before = notices(&h.store).len();
        assert_eq!(decide(&warn, &h.store, &send, owner(), now), Verdict::Allow);
        let told = notices(&h.store);
        assert_eq!(told.len(), before + 1);
        assert_eq!(
            told.last().unwrap(),
            "[zephyr] the agent sent your private fact phone to alice@example.com"
        );
        // `off` checks nothing.
        let off = config(&h.dir, "[approvals]\npreset = 'full'\nfacts = 'off'\n");
        assert_eq!(decide(&off, &h.store, &send, owner(), now), Verdict::Allow);
        assert_eq!(
            outcomes(&h.store)[5..],
            ["fact-warn".to_string(), "auto".to_string()]
        );
        // Facts that cannot be read hold every call they would be looked for in, unless off.
        std::fs::write(crate::facts::path(&h.cfg), "[[fact]]\nname = 3 x\n").unwrap();
        let plain_send = exec(&mail_to("alice@example.com", "Hi."), "main:6");
        for cfg in [&h.cfg, &warn] {
            let v = decide(cfg, &h.store, &plain_send, owner(), now);
            let Verdict::Hold { id, text } = v else {
                panic!("{v:?}")
            };
            // Nothing of the call can be checked for a fact, so nothing of it is shown.
            assert_eq!(text, held_text(id, "your private facts could not be read"));
            let card = notices(&h.store).pop().unwrap();
            assert!(
                card.contains("\nyour private facts could not be read\n"),
                "{card}"
            );
            assert!(card.starts_with(&format!("[zephyr] approve #{id}: message (email)\n")));
            assert!(!card.contains("alice@") && !card.contains("Hi."), "{card}");
        }
        assert_eq!(
            decide(&off, &h.store, &plain_send, owner(), now),
            Verdict::Allow
        );
        // C2: facts are looked for in the values a call carries, decoded, never in its JSON text
        // (where a newline is the two characters `\n`), and scrubbed there too.
        std::fs::write(crate::facts::path(&h.cfg), format!("{PHONE}{ADDR}")).unwrap();
        let values = [
            json!({"address": "12 Oak Street,\nSpringfield"}),
            json!({"phone": "555\n010\n2345"}),
            json!({"lines": ["12 Oak Street,", "Springfield"]}),
            // Two fields of one number: their keys must not break the digit run.
            json!({"area": "555", "number": "010-2345"}),
        ];
        for (i, arguments) in values.into_iter().enumerate() {
            let req = mcp("crm", "update_contact", arguments, &format!("facts:{i}"));
            held(&decide(&h.cfg, &h.store, &req, owner(), now));
            let card = notices(&h.store).pop().unwrap();
            let (_, _, args, outcome) = h.store.audit_rows().unwrap().pop().unwrap();
            assert_eq!(outcome, "fact-hold");
            for shown in [&card, &args] {
                for value in ["Oak Street", "Springfield", "010", "2345"] {
                    assert!(!shown.contains(value), "{value} in {shown}");
                }
            }
        }
        // A heredoc's two-line address is held, and its audit row holds no part of it.
        let letter = mail_to(
            "alice@example.com",
            "I live at 12 Oak Street,\nSpringfield.",
        );
        let id = held(&decide(
            &h.cfg,
            &h.store,
            &exec(&letter, "facts:mail"),
            owner(),
            now,
        ));
        assert!(action(&h.store, id)
            .summary
            .ends_with("contains your private fact addr"));
        let (_, _, args, _) = h.store.audit_rows().unwrap().pop().unwrap();
        assert!(
            args.contains("[fact:addr]") && !args.contains("Oak Street"),
            "{args}"
        );
        // A browser card shows what was typed scrubbed, never the value it names.
        let mut typing = click("", "https://forms.example.com", "facts:typing");
        if let Some(b) = typing.browser.as_mut() {
            b.action = "fill".into();
            b.typed = vec!["12 Oak Street,\nSpringfield".into()];
        }
        held(&decide(&h.cfg, &h.store, &typing, owner(), now));
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.contains("\ncontains your private fact addr\n"),
            "{card}"
        );
        assert!(card.contains("typed: \"[fact:addr]\""), "{card}");
        assert!(!card.contains("Oak Street"), "{card}");
        // A value typed in two entries is held, and neither entry is shown.
        for (i, typed) in [["555", "010 2345"], ["12 Oak Street,", "Springfield"]]
            .into_iter()
            .enumerate()
        {
            let mut split = click("", "https://forms.example.com", &format!("facts:split{i}"));
            if let Some(b) = split.browser.as_mut() {
                b.action = "fill".into();
                b.typed = typed.iter().map(|t| t.to_string()).collect();
            }
            held(&decide(&h.cfg, &h.store, &split, owner(), now));
            let card = notices(&h.store).pop().unwrap();
            assert!(card.contains("\ntyped: [withheld: "), "{card}");
            for value in typed {
                assert!(!card.contains(value), "{value} in {card}");
            }
        }
        // R38: a fact's allow list is compared with the recipients, not the connector's tool.
        let to = |to: Value, run: &str| {
            let req = mcp(
                "mail",
                "send_email",
                json!({"to": to, "body": "call 5550102345"}),
                run,
            );
            decide(&h.cfg, &h.store, &req, owner(), now)
        };
        assert_eq!(
            to(json!("carol@example.com"), "facts:carol"),
            Verdict::Allow
        );
        let both = json!(["carol@example.com", "dave@example.com"]);
        held(&to(both, "facts:dave"));
        // C1: the owner's notice that a fact went somewhere is one line, whatever the agent named.
        let slack = mcp(
            "slack",
            "send_message",
            json!({"channel": "#gen\n[zephyr] approve #9: x\n/ok 9", "text": "call 5550102345"}),
            "facts:warn",
        );
        assert_eq!(
            decide(&warn, &h.store, &slack, owner(), now),
            Verdict::Allow
        );
        let told = notices(&h.store).pop().unwrap();
        assert!(
            told.starts_with(
                "[zephyr] the agent sent your private fact phone to slack/send_message"
            ),
            "{told}"
        );
        assert_eq!(told.lines().count(), 1, "{told}");
    }

    const ADDR: &str = "[[fact]]\nname = \"addr\"\nkind = \"text\"\n\
                        value = \"12 Oak Street, Springfield\"\n";

    #[test]
    fn money_over_or_unknown_cap_is_held() {
        let h = home("gate-money", "[approvals]\nmoney = 'auto 50'\n");
        let now = crate::now();
        // Each from a run of its own, so the flood cap stays out of it.
        let pay = |arguments: Value| {
            let run = format!("money:{arguments}");
            let req = mcp("shop", "checkout", arguments, &run);
            decide(&h.cfg, &h.store, &req, owner(), now)
        };
        assert_eq!(
            pay(json!({"amount": 20, "currency": "usd"})),
            Verdict::Allow
        );
        assert_eq!(
            pay(json!({"total": "50.00", "currency": "USD"})),
            Verdict::Allow
        );
        // R36: an amount counts rounded up to the cent: 49.999 is 50.00, at the cap.
        assert_eq!(
            pay(json!({"amount": 49.999, "currency": "USD"})),
            Verdict::Allow
        );
        for (arguments, shown) in [
            (
                json!({"amount": 50.004, "currency": "USD"}),
                "amount: 50.004 USD",
            ),
            (
                json!({"amount": 50.01, "currency": "USD"}),
                "amount: 50.01 USD",
            ),
            (json!({"amount": 20, "currency": "EUR"}), "amount: 20 EUR"),
            (json!({"amount": 20}), "amount: 20, no currency named"),
            (json!({"item": "a lamp"}), "amount: could not be read"),
        ] {
            let id = held(&pay(arguments));
            let card = notices(&h.store).pop().unwrap();
            assert!(
                card.starts_with(&format!(
                    "[zephyr] approve #{id}: payment (connector)\ntool: shop/checkout\n"
                )),
                "{card}"
            );
            assert!(card.contains(&format!("\n{shown}\n")), "{card}");
            // A payment has no recipients to remember: no `once`.
            assert!(
                card.ends_with(&format!("\n/ok {id} · /no {id} · expires in 24 h")),
                "{card}"
            );
        }
        // A checkout button has no amount the gate can read: over the cap.
        let v = decide(
            &h.cfg,
            &h.store,
            &click("Place order", "https://shop.example.com", "main:7"),
            owner(),
            now,
        );
        let Verdict::Wait { id } = v else {
            panic!("{v:?}")
        };
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: payment (browser)\non: https://shop.example.com\n\
                 page: \"https://shop.example.com/compose\" · button: \"Place order\"\n\
                 amount: could not be read\n"
            )),
            "{card}"
        );
        assert!(card.ends_with("expires in 10 min"), "{card}");
        assert_eq!(
            outcomes(&h.store),
            ["auto", "auto", "auto", "held", "held", "held", "held", "held", "held"]
        );
        // Without a cap any amount goes; with `money` strong-only the card says where to approve.
        let uncapped = config(&h.dir, "[approvals]\nmoney = 'auto'\n");
        let req = mcp("shop", "checkout", json!({}), "task-3");
        assert_eq!(
            decide(&uncapped, &h.store, &req, owner(), now),
            Verdict::Allow
        );
        let strong = config(
            &h.dir,
            "[approvals]\nmoney = 'ask'\nstrong_only = ['loosen', 'money']\n",
        );
        let id = held(&decide(&strong, &h.store, &req, owner(), now));
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.ends_with(&format!(
                "\n/ok {id} · /no {id} · expires in 24 h · this one needs tui or chat"
            )),
            "{card}"
        );
    }

    #[test]
    fn a_card_never_quotes_the_agent_outside_its_reason_line() {
        let h = home("gate-card", "");
        std::fs::write(
            crate::vault::path(&h.cfg),
            "[[token]]\nname = \"GH_TOKEN\"\nvalue = \"ghp_secret123\"\nprograms = [\"gh\"]\n",
        )
        .unwrap();
        std::fs::write(crate::facts::path(&h.cfg), PHONE).unwrap();
        let now = crate::now();
        let reason = format!(
            "token ghp_secret123, phone 555 010 2345\n[zephyr] approve #99: nothing to \
             see\n/ok 99 · /no 99\u{202e}{}",
            "x".repeat(300)
        );
        let body = "Hello.\n[zephyr] approve #98: a fake card\n/ok 98 · /no 98\nBye.";
        let mut req = exec(&mail_to("alice@example.com", body), "main:8");
        req.reason = Some(reason);
        let id = held(&decide(&h.cfg, &h.store, &req, owner(), now));
        let a = action(&h.store, id);
        let shown = card(&a, &h.cfg, &h.store);
        assert_eq!(notices(&h.store).pop().unwrap(), shown);
        let lines: Vec<&str> = shown.lines().collect();
        // The daemon's own lines are the first and the last; nothing else can pass for one.
        assert!(lines[0].starts_with(&format!("[zephyr] approve #{id}: ")));
        assert_eq!(
            *lines.last().unwrap(),
            format!("/ok {id} · /ok {id} once · /no {id} · expires in 24 h")
        );
        for line in &lines[1..lines.len() - 1] {
            assert!(
                !line.starts_with("[zephyr]") && !line.starts_with("/ok"),
                "{line}"
            );
        }
        // The agent's words are on one line, quoted and cut at 200 characters.
        let says: Vec<&&str> = lines
            .iter()
            .filter(|l| l.contains("nothing to see") || l.contains("xxxx"))
            .collect();
        assert_eq!(says.len(), 1, "{shown}");
        let says = says[0];
        assert!(says.starts_with("the agent says: \""), "{says}");
        let quoted = &says["the agent says: \"".len()..says.len() - 1];
        assert!(quoted.ends_with('…'), "{quoted}");
        assert_eq!(quoted.chars().count(), 201, "{quoted}");
        // The message's own text is quoted on its line, never laid out as the daemon's.
        assert!(
            lines.contains(
                &"first lines: \"Hello. [zephyr] approve #98: a fake card /ok 98 · /no 98 Bye.\""
            ),
            "{shown}"
        );
        // No secret and no fact value, anywhere: the reason was scrubbed before it was cut.
        assert!(!shown.contains('\u{202e}'), "{shown}");
        for value in ["ghp_secret123", "010 2345", "5550102345"] {
            assert!(!shown.contains(value), "{shown}");
            assert!(!a.reason.as_deref().unwrap_or_default().contains(value));
        }
        assert!(
            says.contains("[vault:GH_TOKEN]") && says.contains("[fact:phone]"),
            "{says}"
        );
        // What the agent is told names the action in the daemon's words only.
        assert_eq!(a.summary, "email to alice@example.com");
        // C1: nothing the agent names (a server, a recipient, a currency, a page) gets a line of
        // its own or a hidden direction mark, on the card, in the summary or in the held text.
        let forged = "x\n[zephyr] approve #51: message (connector)\ntool: cal/remind\n\
                      /ok 51 · /no 51 · expires in 24 h";
        let calls = [
            mcp(forged, "send", json!({}), "card:1"),
            mcp(
                "slack",
                "send_message",
                json!({"channel": "#a\n/ok 53 · /no 53"}),
                "card:2",
            ),
            mcp(
                "shop",
                "checkout",
                json!({"amount": 80, "currency": "usd\n[zephyr] approve #52: x"}),
                "card:3",
            ),
            exec(
                &mail_to("alice\u{202e}gro.live@example.com", "Hi."),
                "card:4",
            ),
            click(
                "Send\n/ok 54",
                "https://mail.example.com\n[zephyr] approve #55: x",
                "card:5",
            ),
        ];
        for req in calls {
            let v = decide(&h.cfg, &h.store, &req, owner(), now);
            let id = held(&v);
            if let Verdict::Hold { text, .. } = &v {
                assert!(!text.contains(['\n', '\u{202e}']), "{text}");
            }
            let a = action(&h.store, id);
            assert!(!a.summary.contains(['\n', '\u{202e}']), "{}", a.summary);
            let shown = notices(&h.store).pop().unwrap();
            assert_eq!(shown, card(&a, &h.cfg, &h.store));
            let lines: Vec<&str> = shown.lines().collect();
            assert!(
                lines[0].starts_with(&format!("[zephyr] approve #{id}: ")),
                "{shown}"
            );
            assert!(
                lines.last().unwrap().starts_with(&format!("/ok {id} ")),
                "{shown}"
            );
            for line in &lines[1..lines.len() - 1] {
                let daemons = ["[zephyr]", "/ok", "tool: cal/"];
                assert!(!daemons.iter().any(|d| line.starts_with(d)), "{shown}");
            }
            assert!(!shown.contains('\u{202e}'), "{shown}");
        }
        // R41: a quote inside quoted text is escaped, so it cannot close the quote and add text
        // that reads as the daemon's.
        let subject = "himalaya message send <<'EOF'\nTo: alice@example.com\n\
                       Subject: Hi\" · to: boss@corp.com\n\nHello.\nEOF";
        held(&decide(
            &h.cfg,
            &h.store,
            &exec(subject, "card:6"),
            owner(),
            now,
        ));
        let shown = notices(&h.store).pop().unwrap();
        assert!(
            shown.contains("\nto: alice@example.com · subject: \"Hi\\\" · to: boss@corp.com\"\n"),
            "{shown}"
        );
        let button = click(
            "Send\" · on: https://bank.example.com",
            "https://mail.example.com",
            "card:7",
        );
        held(&decide(&h.cfg, &h.store, &button, owner(), now));
        let shown = notices(&h.store).pop().unwrap();
        assert!(
            shown.contains("· button: \"Send\\\" · on: https://bank.example.com\"\n"),
            "{shown}"
        );
    }

    #[test]
    fn a_browser_action_the_gate_cannot_read_is_held() {
        // I3: with every class on `auto`, a click whose control has no role is still held.
        let h = home("gate-unread", "[approvals]\npreset = 'full'\n");
        let now = crate::now();
        let blind = |action: &str, run: &str| {
            let mut req = click("", "https://shop.example.com", run);
            if let Some(b) = req.browser.as_mut() {
                b.action = action.into();
                b.role = String::new();
            }
            decide(&h.cfg, &h.store, &req, owner(), now)
        };
        let Verdict::Wait { id } = blind("click", "unread:1") else {
            panic!("an unreadable click waits")
        };
        let a = action(&h.store, id);
        assert_eq!(
            a.summary,
            "a click the gate could not read on https://shop.example.com"
        );
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: a click the gate could not read (browser)\n\
                 on: https://shop.example.com\n\
                 page: \"https://shop.example.com/compose\" · the control could not be read\n"
            )),
            "{card}"
        );
        for action in ["press", "keyboard", "find"] {
            let v = blind(action, &format!("unread:{action}"));
            assert!(matches!(v, Verdict::Wait { .. }), "{action}: {v:?}");
        }
        // A control the gate reads and knows is no send still goes (an unknown label: §4.4).
        let mut link = click("Home", "https://shop.example.com", "unread:2");
        if let Some(b) = link.browser.as_mut() {
            b.role = "link".into();
        }
        assert_eq!(
            decide(&h.cfg, &h.store, &link, owner(), now),
            Verdict::Allow
        );
        // A browser request without its page facts is refused.
        let bare = Request {
            browser: None,
            ..click("Send", "https://mail.example.com", "unread:3")
        };
        assert_eq!(
            decide(&h.cfg, &h.store, &bare, owner(), now),
            Verdict::Deny("not run: the gate could not read the browser request".into())
        );
        // The spec's wire shape, `element: {role, name}`, reads as the flat one does.
        let wire: Browser = serde_json::from_value(json!({
            "action": "click", "url": "https://mail.example.com/c",
            "origin": "https://mail.example.com",
            "element": {"role": "button", "name": "Send"}, "typed": []
        }))
        .unwrap();
        assert_eq!((wire.role.as_str(), wire.name.as_str()), ("button", "Send"));
        let manual = config(&h.dir, "");
        let req = Request {
            browser: Some(wire),
            ..click("", "", "unread:4")
        };
        let Verdict::Wait { id } = decide(&manual, &h.store, &req, owner(), now) else {
            panic!("a send button waits under manual")
        };
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!("[zephyr] approve #{id}: message (browser)\n")),
            "{card}"
        );
        assert_eq!(
            outcomes(&h.store),
            ["held", "held", "held", "held", "denied", "held"]
        );
        // R39: no role but a readable name keeps the class the name gives; still held, and the
        // card shows the button and what a payment needs.
        let strong = config(
            &h.dir,
            "[approvals]\npreset = 'full'\nstrong_only = ['loosen', 'money']\n",
        );
        let mut buy = click("Place order", "https://shop.example.com", "unread:5");
        if let Some(b) = buy.browser.as_mut() {
            b.role = String::new();
        }
        let Verdict::Wait { id } = decide(&strong, &h.store, &buy, owner(), now) else {
            panic!("an unreadable order button waits")
        };
        assert_eq!(action(&h.store, id).class, "money");
        let card = notices(&h.store).pop().unwrap();
        assert!(
            card.starts_with(&format!(
                "[zephyr] approve #{id}: payment (browser)\non: https://shop.example.com\n\
                 page: \"https://shop.example.com/compose\" · button: \"Place order\" (its role \
                 could not be read)\namount: could not be read\n"
            )),
            "{card}"
        );
        assert!(card.ends_with("· this one needs tui or chat"), "{card}");
    }

    #[test]
    fn an_approved_connector_call_passes_once() {
        let h = home("gate-pass", "");
        let now = crate::now();
        let req = mcp(
            "mail",
            "send_email",
            json!({"to": "a@x.org", "body": "hi"}),
            "task-3",
        );
        let id = held(&decide(&h.cfg, &h.store, &req, owner(), now));
        let a = action(&h.store, id);
        assert_eq!(a.kind, ActionKind::Pass);
        let stored: Value = serde_json::from_str(&a.payload).unwrap();
        assert_eq!(stored, req.args);
        let digest = pass_digest(&stored).unwrap();
        // Bound to the server, the tool and the arguments, whatever order their keys come in.
        let reordered = json!({"tool": "send_email", "arguments": {"body": "hi", "to": "a@x.org"},
                               "server": "mail"});
        assert_eq!(pass_digest(&reordered).as_deref(), Some(digest.as_str()));
        let other = json!({"server": "mail", "tool": "send_email",
                           "arguments": {"to": "b@x.org", "body": "hi"}});
        assert_ne!(pass_digest(&other).as_deref(), Some(digest.as_str()));
        h.store.add_pass(&digest, id, now + 1800).unwrap();
        assert_eq!(decide(&h.cfg, &h.store, &req, owner(), now), Verdict::Allow);
        held(&decide(&h.cfg, &h.store, &req, owner(), now));
        assert_eq!(outcomes(&h.store), ["held", "pass", "held"]);
    }
}
