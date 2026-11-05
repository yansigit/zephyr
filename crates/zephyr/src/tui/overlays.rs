//! Panels over the bottom of the screen (the model picker, the status panel and help) and
//! slash autocomplete above the composer. Each draws from the shared [`State`]; mod.rs routes
//! the keys here while one is open.

use super::chrome::{short_model, spend, State};
use super::conn::{AccountModels, Login, LoginStatus, Provider, Snapshot, VaultEntry};
use super::theme::{glyph, Palette};
use super::view::clean;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_tui::{bold, truncate_to_width, visible_width, CURSOR_MARKER};

/// Every slash command zephyr answers: router.rs `command` and `control`, memory.rs
/// `command` and scheduler.rs `command`, plus the TUI's own `/help`, then the TUI's keys, which help
/// lists but does not insert. (group, usage, what it does)
pub const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "Chat",
        "/new",
        "start a fresh conversation; memory and rules carry over",
    ),
    ("Chat", "/switch", "list earlier conversations"),
    ("Chat", "/switch <id>", "go back to one"),
    (
        "Chat",
        "/btw <question>",
        "ask a side question; the conversation is left as is",
    ),
    ("Chat", "/undo", "take back your last message and its reply"),
    ("Chat", "/retry", "take it back and send it again"),
    (
        "Chat",
        "/compact [instructions]",
        "summarise the conversation to free context",
    ),
    (
        "Chat",
        "/shell [zsh]",
        "a shell in the container, as the agent",
    ),
    (
        "Chat",
        "!<command>",
        "run it; the next message carries its output",
    ),
    ("Chat", "/copy", "copy the last reply"),
    ("Chat", "/copy <n>", "copy code block n, or click it"),
    ("Chat", "/focus", "hide tool calls and thinking, or show them"),
    ("Chat", "/help", "this list"),
    ("Chat", "/quit", "leave (or /exit); zephyr keeps running"),
    ("Agent", "/stop", "stop the current reply and every task"),
    ("Agent", "/task list", "the agent's background tasks"),
    ("Agent", "/task stop <id>", "stop one task"),
    (
        "Agent",
        "/process list",
        "commands running in the background",
    ),
    ("Agent", "/process stop <id>", "stop one command"),
    ("Agent", "/model", "pick a model"),
    (
        "Agent",
        "/model <provider/model>",
        "switch the model after the current reply",
    ),
    ("Agent", "/rollback", "file checkpoints of the projects"),
    (
        "Agent",
        "/rollback diff <n>",
        "what changed since checkpoint n",
    ),
    (
        "Agent",
        "/rollback <n> [project]",
        "put the files back as at n",
    ),
    ("Agent", "/ok <n>", "run the agent's proposal n"),
    ("Agent", "/no <n>", "drop the agent's proposal n"),
    (
        "Memory",
        "/memory diff",
        "the memory change waiting for you",
    ),
    (
        "Memory",
        "/memory ok",
        "let the agent see the waiting change",
    ),
    (
        "Memory",
        "/memory undo",
        "drop the waiting change, else the last one",
    ),
    ("Memory", "/memory log", "the last 10 memory commits"),
    ("Memory", "/skills", "approved skills and waiting drafts"),
    (
        "Memory",
        "/skill ok <name>",
        "approve the agent's skill draft",
    ),
    ("Memory", "/skill no <name>", "drop a skill draft"),
    (
        "Memory",
        "/skill diff|rm|undo <name>",
        "show, remove or revert a skill",
    ),
    ("Memory", "/mcp", "connectors and waiting drafts"),
    (
        "Memory",
        "/mcp ok|no|diff|rm <name>",
        "approve, drop, show or remove one",
    ),
    ("Memory", "/rules", "your standing rules"),
    ("Memory", "/rule <text>", "add a rule the agent reads first"),
    ("Memory", "/rule rm <n>", "remove rule n"),
    ("Schedule", "/job list", "the scheduled jobs"),
    (
        "Schedule",
        "/job add <name> | <cron or time> | <prompt>",
        "schedule a prompt",
    ),
    ("Schedule", "/job run <id>", "run a job now"),
    ("Schedule", "/job rm <id>", "remove a job"),
    (
        "Vault",
        "/vault",
        "saved logins and tokens: add, edit, delete",
    ),
    (
        "Vault",
        "/vault add login <name> <site>",
        "save a login the agent can fill",
    ),
    (
        "Vault",
        "/vault add token <name>",
        "save a token for the agent's commands",
    ),
    (
        "Vault",
        "/vault revoke <name> [program]",
        "take back what the agent may use",
    ),
    ("Vault", "/vault rm <name>", "delete an entry"),
    (
        "Gateway",
        "/status",
        "agent, accounts, channels, spend and jobs",
    ),
    ("Gateway", "/usage [days]", "spend per day and account"),
    (
        "Gateway",
        "/login",
        "sign in to a provider and add its account",
    ),
    ("Gateway", "/pause", "hold new messages until /resume"),
    ("Gateway", "/resume", "answer what waited"),
    ("Gateway", "/reload", "apply zephyr.toml and secrets.env"),
    (
        "Keys",
        "Ctrl+F",
        "search the conversation; Enter older, Shift+Enter newer",
    ),
    ("Keys", "Ctrl+Up/Down", "jump between your messages"),
    ("Keys", "Home/End", "top or bottom, when the composer is empty"),
    ("Keys", "Shift+click", "select from the last click and copy"),
    ("Keys", "double-click", "copy the word; triple-click the row"),
];

/// What goes in the composer for a command: up to its first argument.
pub fn insert(usage: &str) -> &str {
    usage.find(['<', '[']).map_or(usage, |i| &usage[..i])
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Model,
    Status,
    Help,
    /// Provider sign-in: the list, then the sign-in running.
    Login,
    /// The credential vault: its entries, and a form to add or edit one (DESIGN §4.15).
    Vault,
}

/// `Kind::Vault`'s own state.
#[derive(Debug, Clone, Default)]
pub struct VaultPanel {
    /// The daemon's latest list; `None` until it came.
    pub entries: Option<Vec<VaultEntry>>,
    /// Why the last call was refused.
    pub problem: Option<String>,
    /// The entry being added or edited.
    pub form: Option<Form>,
    /// The entry `d` asked to delete, until `y` or any other key.
    pub confirm: Option<String>,
}

/// The add/edit form. A secret field is drawn one bullet per character, handed over in the
/// `vault_put` the moment the form is saved, and dropped with it: never shown, logged or kept.
#[derive(Clone, PartialEq)]
pub struct Form {
    pub kind: crate::vault::Kind,
    /// Editing a saved entry: its name stays, and an empty secret keeps the saved one.
    pub editing: bool,
    /// The field being typed into.
    pub at: usize,
    /// `(label, secret, text)` per field.
    pub fields: Vec<(&'static str, bool, String)>,
}

impl std::fmt::Debug for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fields: Vec<String> = (self.fields.iter())
            .map(|(label, secret, text)| match secret {
                true => format!("{label}: {} chars", text.chars().count()),
                false => format!("{label}: {text:?}"),
            })
            .collect();
        f.debug_struct("Form")
            .field("kind", &self.kind)
            .field("editing", &self.editing)
            .field("at", &self.at)
            .field("fields", &fields)
            .finish()
    }
}

impl Form {
    /// A blank form for `kind`, with what is known filled in: `[name, site, username]`.
    pub fn new(kind: crate::vault::Kind, known: [&str; 3]) -> Form {
        let [name, site, user] = known.map(String::from);
        let fields = match kind {
            crate::vault::Kind::Login => vec![
                ("name", false, name),
                ("site", false, site),
                ("username", false, user),
                ("password", true, String::new()),
                ("totp seed", true, String::new()),
            ],
            crate::vault::Kind::Token => {
                vec![("name", false, name), ("value", true, String::new())]
            }
        };
        // The first field still to type.
        let at = fields.iter().position(|f| f.2.is_empty()).unwrap_or(0);
        Form {
            kind,
            editing: false,
            at,
            fields,
        }
    }

    /// The form `/vault add login NAME SITE` or `/vault add token NAME` opens, filled in.
    pub fn from_command(text: &str) -> Option<Form> {
        let words: Vec<&str> = text.split_whitespace().collect();
        match words[..] {
            ["/vault", "add", "login", ref rest @ ..] if rest.len() <= 2 => Some(Form::new(
                crate::vault::Kind::Login,
                [
                    rest.first().copied().unwrap_or(""),
                    rest.get(1).copied().unwrap_or(""),
                    "",
                ],
            )),
            ["/vault", "add", "token", ref rest @ ..] if rest.len() <= 1 => Some(Form::new(
                crate::vault::Kind::Token,
                [rest.first().copied().unwrap_or(""), "", ""],
            )),
            ["/vault", "add"] => Some(Form::new(crate::vault::Kind::Login, ["", "", ""])),
            _ => None,
        }
    }

    /// The form to edit `e`: everything but the secrets, which an empty field keeps.
    fn edit(e: &VaultEntry) -> Form {
        let kind = match e.kind.as_str() {
            "token" => crate::vault::Kind::Token,
            _ => crate::vault::Kind::Login,
        };
        let mut form = Form::new(kind, [&e.name, &e.site, &e.username]);
        form.editing = true;
        form.at = form.fields.iter().position(|f| f.1).unwrap_or(1);
        form
    }

    fn get(&self, label: &str) -> String {
        let field = self.fields.iter().find(|f| f.0 == label);
        field.map(|f| f.2.trim().to_string()).unwrap_or_default()
    }

    /// The entry to save. Consumes the form, so the typed secrets go with it.
    fn entry(self) -> crate::vault::Entry {
        crate::vault::Entry {
            kind: self.kind,
            name: self.get("name"),
            site: self.get("site"),
            username: self.get("username"),
            password: self.get("password"),
            totp: self.get("totp seed"),
            value: self.get("value"),
            ..Default::default()
        }
    }

    /// Typing goes to the field at the cursor; an entry's name is fixed while it is edited.
    fn type_text(&mut self, text: &str) {
        if self.editing && self.at == 0 {
            return;
        }
        let one_line = text.replace(['\r', '\n'], "");
        self.fields[self.at].2.push_str(&one_line);
    }
}

/// `Kind::Login`'s own state: what the daemon offers to sign in to, and the sign-in running.
#[derive(Clone, Default)]
pub struct SignIn {
    /// `zephyr.login_providers`' answer. `None` until it came, then possibly empty: an older
    /// daemon does not know the method, and says so with an error the UI cannot show here.
    pub providers: Option<Vec<Provider>>,
    /// The sign-in `zephyr.login_start` opened, while it runs.
    pub session: Option<Login>,
    /// The provider whose `login_start` the panel is waiting for. Only one start is ever
    /// outstanding, so a second Enter cannot open a sign-in the panel has nowhere to show.
    pub requested: Option<String>,
    /// Its latest `zephyr.login_poll` answer.
    pub status: Option<LoginStatus>,
    /// Why the last sign-in here ended without the daemon's saying so — it stopped answering
    /// the polls. Said above the list, where the user is looking, rather than in the
    /// transcript, since the panel is what was frozen.
    pub problem: Option<String>,
    /// The token typed at a `need_input` prompt. Drawn one bullet per character, cleared the
    /// moment it is sent, and never shown, logged or stored anywhere else.
    typed: String,
}

impl SignIn {
    /// The running sign-in's id: `None` when none was started, or the last one ended. It runs
    /// from `login_start`'s answer until a poll says `done` or `failed` — the first polls carry
    /// no status yet, so the session alone decides.
    pub fn running(&self) -> Option<&str> {
        self.session.as_ref().map(|s| s.id.as_str())
    }

    /// Whether the daemon waits for a token from the keyboard.
    pub fn needs_input(&self) -> bool {
        self.running().is_some()
            && self
                .status
                .as_ref()
                .is_some_and(|s| s.phase == "need_input")
    }

    /// Asks the daemon to sign this provider in, unless one already runs or its start is still
    /// unanswered. `true` when the request should go out.
    pub fn ask(&mut self, provider: &str) -> bool {
        if self.running().is_some() || self.requested.is_some() {
            return false;
        }
        self.reset();
        self.requested = Some(provider.to_string());
        true
    }

    /// Takes on the sign-in `login_start` answered with. `false` for one this panel did not
    /// ask for: a stale start (the panel was closed and reopened) must not attach itself to
    /// the sign-in on screen now. The caller cancels that one with the daemon, since nothing
    /// here would ever poll or drop it.
    pub fn adopt(&mut self, login: &Login) -> bool {
        if self.requested.as_deref() != Some(login.provider.as_str()) || self.running().is_some() {
            return false;
        }
        self.requested = None;
        self.session = Some(login.clone());
        self.status = None;
        true
    }

    /// Forgets the sign-in and goes back to the provider list.
    pub fn reset(&mut self) {
        self.session = None;
        self.requested = None;
        self.status = None;
        self.problem = None;
        self.typed.clear();
    }

    fn type_char(&mut self, c: char) {
        self.typed.push(c);
    }

    fn backspace(&mut self) {
        self.typed.pop();
    }

    /// Takes the typed token: the caller sends it, and the buffer is empty from then on.
    fn take(&mut self) -> String {
        std::mem::take(&mut self.typed)
    }

    /// The masked value: one bullet per character, so onlookers learn only its length.
    fn masked(&self) -> String {
        glyph("•").repeat(self.typed.chars().count())
    }
}

impl std::fmt::Debug for SignIn {
    /// The typed token is never printed, not even by `{:?}`.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SignIn")
            .field("providers", &self.providers)
            .field("session", &self.session)
            .field("requested", &self.requested)
            .field("status", &self.status)
            .field("problem", &self.problem)
            .field("typed", &self.typed.chars().count())
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct Overlay {
    pub kind: Kind,
    pub query: String,
    /// The chosen row, counted among the pickable ones.
    pub selected: usize,
    /// The `zephyr.models` answer, once it came.
    pub models: Option<Vec<AccountModels>>,
    /// The selection starts on the current model; set again when the models arrive.
    pub placed: bool,
    /// The login picker's own state.
    pub login: SignIn,
    /// The vault panel's own state.
    pub vault: VaultPanel,
    /// The first row shown and how many fit, from the last frame.
    top: usize,
    page: usize,
}

impl Overlay {
    pub fn new(kind: Kind) -> Self {
        Overlay {
            kind,
            query: String::new(),
            selected: 0,
            models: None,
            placed: false,
            login: SignIn::default(),
            vault: VaultPanel::default(),
            top: 0,
            page: 1,
        }
    }
}

/// What a key did in a panel.
#[derive(Debug, PartialEq)]
pub enum Action {
    None,
    Close,
    /// The model picker's `/model …` to send, or help's command for the composer.
    Pick(String),
    /// Start this provider's sign-in (its id, from the login picker).
    Login(String),
    /// The token typed at a `need_input` prompt, to write to the login process.
    Input(String),
    /// Give up on this sign-in and close the panel.
    Cancel(String),
    /// Save this vault entry.
    VaultPut(Box<crate::vault::Entry>),
    /// Delete this vault entry.
    VaultRm(String),
    /// Take back every grant of this vault entry.
    VaultRevoke(String),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    /// A group heading and a muted note.
    Head(String, String),
    /// A choice: what picking it gives, its label, a muted note, and whether it is in use.
    Item {
        pick: String,
        label: String,
        note: String,
        current: bool,
    },
    /// A styled line (the status panel).
    Line(String),
}

/// The panel's rows for `snap`, filtered by the query. `now` is in milliseconds; `width` is
/// what the panel has, so a URL too long for one row can be broken instead of cut.
pub fn rows(
    o: &Overlay,
    snap: &Snapshot,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    now: i64,
    width: usize,
) -> Vec<Row> {
    let groups = match o.kind {
        Kind::Status => return status_rows(snap, p, tz, now),
        Kind::Login => return login_rows(o, p, width),
        Kind::Vault => return vault_rows(o, p),
        Kind::Help => help_groups(),
        Kind::Model => model_groups(o, snap, tz, now),
    };
    let words: Vec<String> = o.query.split_whitespace().map(str::to_lowercase).collect();
    let mut out = vec![];
    for (head, note, items) in groups {
        let hit: Vec<Row> = items
            .into_iter()
            .filter(|r| {
                let Row::Item {
                    pick, label, note, ..
                } = r
                else {
                    return true;
                };
                let hay = format!("{head} {label} {note} {pick}").to_lowercase();
                words.iter().all(|w| hay.contains(w.as_str()))
            })
            .collect();
        if !hit.is_empty() {
            out.push(Row::Head(head, note));
            out.extend(hit);
        }
    }
    if out.is_empty() {
        out.push(Row::Line(p.muted.fg("nothing matches")));
    }
    if o.kind == Kind::Model && o.models.is_none() {
        out.push(Row::Line(
            p.muted.fg(&format!("listing models{}", glyph("…"))),
        ));
    }
    out
}

type Group = (String, String, Vec<Row>);

fn help_groups() -> Vec<Group> {
    let mut groups: Vec<Group> = vec![];
    for (group, usage, what) in COMMANDS {
        if groups.last().is_none_or(|g| g.0 != *group) {
            groups.push((group.to_string(), String::new(), vec![]));
        }
        let item = Row::Item {
            pick: insert(usage).to_string(),
            label: usage.to_string(),
            note: what.to_string(),
            current: false,
        };
        groups.last_mut().unwrap().2.push(item);
    }
    groups
}

/// One group per account, or per provider without a pool. Until `zephyr.models` answers,
/// each account shows the model it is set to.
fn model_groups(o: &Overlay, snap: &Snapshot, tz: &jiff::tz::TimeZone, now: i64) -> Vec<Group> {
    let fallback = || -> Vec<AccountModels> {
        if snap.accounts.is_empty() {
            return vec![AccountModels {
                name: None,
                model: snap.model.clone(),
                models: snap.model.iter().cloned().collect(),
            }];
        }
        (snap.accounts.iter())
            .map(|a| AccountModels {
                name: Some(a.name.clone()),
                model: Some(a.model.clone()),
                models: vec![a.model.clone()],
            })
            .collect()
    };
    let lists = o
        .models
        .clone()
        .filter(|m| !m.is_empty())
        .unwrap_or_else(fallback);
    let item = |id: &str, current: bool| Row::Item {
        pick: format!("/model {id}"),
        label: short_model(id).to_string(),
        note: String::new(),
        current,
    };
    let mut groups: Vec<Group> = vec![];
    for a in lists {
        match &a.name {
            Some(name) => {
                let in_use = snap.account.as_deref() == Some(name);
                let note = match snap.accounts.iter().find(|s| &s.name == name) {
                    _ if in_use => "in use".to_string(),
                    Some(s) => account_state(s, tz, now / 1000),
                    None => String::new(),
                };
                let items = (a.models.iter())
                    .map(|m| item(m, in_use && snap.model.as_ref() == Some(m)))
                    .collect();
                groups.push((name.clone(), note, items));
            }
            // Without a pool there is one entry: group its models by provider.
            None => {
                for m in &a.models {
                    let provider = m.split('/').next().unwrap_or_default().to_string();
                    if groups.last().is_none_or(|g| g.0 != provider) {
                        groups.push((provider, String::new(), vec![]));
                    }
                    let current = snap.model.as_ref() == Some(m);
                    groups.last_mut().unwrap().2.push(item(m, current));
                }
            }
        }
    }
    groups
}

fn account_state(a: &super::conn::AccountState, tz: &jiff::tz::TimeZone, now: i64) -> String {
    match (a.state.as_str(), a.until_unix) {
        ("cooling", Some(until)) => format!("cooling until {}", when(until, tz, now)),
        (state, _) => state.to_string(),
    }
}

/// `15:00` today, `Fri 09:00` within a week, else `6 Nov 09:00`.
fn when(unix: i64, tz: &jiff::tz::TimeZone, now: i64) -> String {
    let at = |t: i64| jiff::Timestamp::from_second(t).map(|t| t.to_zoned(tz.clone()));
    let (Ok(t), Ok(n)) = (at(unix), at(now)) else {
        return unix.to_string();
    };
    let format = if t.date() == n.date() {
        "%H:%M"
    } else if (unix - now).abs() < 6 * 24 * 3600 {
        "%a %H:%M"
    } else {
        "%-d %b %H:%M"
    };
    t.strftime(format).to_string()
}

fn status_rows(snap: &Snapshot, p: &Palette, tz: &jiff::tz::TimeZone, now: i64) -> Vec<Row> {
    let mut out = vec![];
    let mut row = |label: &str, value: String| {
        out.push(Row::Line(format!(
            "{}{value}",
            p.muted.fg(&format!("{label:<10}"))
        )));
    };
    let dot = |g: &'static str| format!(" {} ", p.muted.fg(glyph(g)));

    let model = snap
        .model
        .as_deref()
        .map(short_model)
        .unwrap_or("default model");
    let mut agent = match &snap.account {
        Some(a) => format!("{}{}{}", p.text.fg(a), dot("›"), p.text.fg(model)),
        None => p.text.fg(model),
    };
    if let Some(t) = &snap.thinking {
        agent.push_str(&p.muted.fg(&format!(" {} thinking {t}", glyph("·"))));
    }
    row("agent", agent);
    let run = &snap.run;
    let doing = if run.busy {
        let mut what = match &run.current_tool {
            Some(t) => format!("running {t}"),
            None => "thinking".to_string(),
        };
        if let Some(o) = run.origin.as_ref().filter(|o| o.channel != "tui") {
            what.push_str(&format!(" for {}", o.channel));
        }
        if let Some(since) = run.since_unix_ms {
            let secs = (now - since).max(0) / 1000;
            what.push_str(&format!(" {} {secs}s", glyph("·")));
        }
        p.text.fg(&what)
    } else {
        p.muted.fg("idle")
    };
    row("", doing);

    let name_w = snap
        .accounts
        .iter()
        .map(|a| visible_width(&a.name))
        .max()
        .unwrap_or(0);
    let model_w = snap
        .accounts
        .iter()
        .map(|a| visible_width(short_model(&a.model)))
        .max()
        .unwrap_or(0);
    for (i, a) in snap.accounts.iter().enumerate() {
        let in_use = snap.account.as_deref() == Some(&a.name);
        let (g, state) = match a.state.as_str() {
            _ if in_use => (p.success.fg(glyph("●")), p.success.fg("in use")),
            "ready" => (p.success.fg(glyph("●")), p.muted.fg("ready")),
            "cooling" => (
                p.warning.fg(glyph("◐")),
                p.warning.fg(&account_state(a, tz, now / 1000)),
            ),
            other => (p.error.fg(glyph("○")), p.error.fg(other)),
        };
        let label = if i == 0 { "accounts" } else { "" };
        row(
            label,
            format!(
                "{g} {}  {}  {state}",
                p.text.fg(&format!("{:<name_w$}", a.name)),
                p.muted.fg(&format!("{:<model_w$}", short_model(&a.model))),
            ),
        );
    }

    let channels: Vec<String> = (snap.channels.iter())
        .map(|c| match c.state.as_str() {
            "connected" => format!("{} {}", p.text.fg(&c.name), p.success.fg(glyph("●"))),
            "reconnecting" => format!(
                "{} {} {}",
                p.text.fg(&c.name),
                p.warning.fg(glyph("◐")),
                p.warning.fg("reconnecting")
            ),
            other => format!(
                "{} {}",
                p.muted.fg(&c.name),
                p.muted.fg(&format!("{} {other}", glyph("○")))
            ),
        })
        .collect();
    row("channels", channels.join("   "));

    let gateway = match snap.paused.as_deref() {
        None => p.text.fg("running"),
        Some(why) => {
            let (what, until) = match why.starts_with("budget") {
                true => ("paused for the budget", "until the UTC day ends or /resume"),
                false => ("paused", "new messages wait until /resume"),
            };
            format!(
                "{}{}",
                p.warning.fg(&format!("{} {what}", glyph("⏸"))),
                p.muted.fg(&format!(" {} {until}", glyph("·")))
            )
        }
    };
    row("gateway", gateway);
    row("spend", spend(snap, p));
    row(
        "queue",
        match snap.queue.len() {
            0 => p.muted.fg("empty"),
            n => p.text.fg(&format!("{n} waiting")),
        },
    );
    row(
        "replies",
        match snap.failed_replies {
            0 => p.muted.fg("all delivered"),
            1 => p.error.fg(&format!("{} 1 failed reply", glyph("✗"))),
            n => p.error.fg(&format!("{} {n} failed replies", glyph("✗"))),
        },
    );
    if snap.jobs.is_empty() {
        row("jobs", p.muted.fg("none scheduled"));
    }
    let times: Vec<String> = (snap.jobs.iter())
        .map(|j| when(j.next_unix, tz, now / 1000))
        .collect();
    let time_w = times.iter().map(|t| visible_width(t)).max().unwrap_or(0);
    for (i, (j, t)) in snap.jobs.iter().zip(&times).enumerate() {
        row(
            if i == 0 { "jobs" } else { "" },
            format!(
                "{}  {} {}",
                p.text.fg(&format!("{t:<time_w$}")),
                p.text.fg(&j.name),
                p.muted.fg(&format!("#{}", j.id))
            ),
        );
    }
    out
}

/// How many of the sign-in's last output lines the running panel shows.
const LAST_OUTPUT: usize = 6;
/// The fewest characters of a URL worth putting on a row of their own, beside the label's
/// indent: narrower than this, the row is the head of the URL, clearly cut.
const URL_MIN: usize = 12;

/// The login picker's rows: the providers until a sign-in starts, then that sign-in.
fn login_rows(o: &Overlay, p: &Palette, width: usize) -> Vec<Row> {
    match o.login.running() {
        Some(_) => running_rows(o, p, width),
        None => provider_rows(o, p),
    }
}

/// One row per provider the daemon can sign in to, filtered like the other pickers.
/// A sign-in was started and the daemon has not answered yet, so there is no session to draw:
/// the list stays, with the reason a sign-in the panel was showing is gone above it.
fn provider_rows(o: &Overlay, p: &Palette) -> Vec<Row> {
    let mut out = Vec::new();
    // The user is looking at this panel, so this is where a sign-in that ended without the
    // daemon's saying so is said: there is no reply card for a poll that never came.
    if let Some(problem) = &o.login.problem {
        out.push(Row::Line(p.error.fg(problem)));
    }
    let Some(list) = &o.login.providers else {
        out.push(Row::Line(
            p.muted.fg(&format!("listing providers{}", glyph("…"))),
        ));
        return out;
    };
    if list.is_empty() {
        // A daemon without `zephyr.login_providers` answers it with an error, which the UI
        // cannot show here: say what still works instead of showing an empty panel.
        out.push(Row::Line(p.muted.fg(
            "this daemon cannot sign anyone in; upgrade it, or run `zephyr login`",
        )));
        return out;
    }
    let words: Vec<String> = o.query.split_whitespace().map(str::to_lowercase).collect();
    out.push(Row::Head(
        "sign in".into(),
        "a new account joins the pool".into(),
    ));
    let mut found = false;
    for pr in list {
        let hay = format!("{} {} {}", pr.id, pr.label, pr.how()).to_lowercase();
        if !words.iter().all(|w| hay.contains(w.as_str())) {
            continue;
        }
        found = true;
        out.push(Row::Item {
            pick: pr.id.clone(),
            label: pr.label.clone(),
            note: pr.how().to_string(),
            current: false,
        });
    }
    if !found {
        // The heading would otherwise be all there is, which reads as "no providers".
        out.push(Row::Line(p.muted.fg("nothing matches")));
    }
    out
}

/// A sign-in in flight: which provider and account, where to go, what it printed, and — at a
/// `need_input` prompt — the masked row the token is typed into.
fn running_rows(o: &Overlay, p: &Palette, width: usize) -> Vec<Row> {
    let Some(session) = &o.login.session else {
        return Vec::new();
    };
    let status = o.login.status.clone().unwrap_or_default();
    let label = o
        .login
        .providers
        .iter()
        .flatten()
        .find(|pr| pr.id == session.provider)
        .map(|pr| pr.label.clone())
        .unwrap_or_else(|| session.provider.clone());
    let field = |name: &str, value: String| format!("{}{value}", p.muted.fg(&format!("{name:<9}")));
    let mut out = vec![Row::Line(field("provider", p.text.fg(&label)))];
    let account = session
        .account
        .clone()
        .unwrap_or_else(|| "the next free name".into());
    out.push(Row::Line(field("account", p.muted.fg(&account))));
    if let Some(url) = status.url.as_deref().filter(|u| !u.is_empty()) {
        // Plain text: the terminal may make a link clickable on its own, and a URL cut at the
        // panel edge cannot be copied back together, so it is broken by hand instead. The
        // chunks are measured against what a row of this panel can hold — the border and its
        // padding are drawn around the row — because a chunk wider than that is cut by the
        // renderer, losing characters with no sign that anything is missing.
        let room = width.saturating_sub(4);
        // The label's column when it leaves room for a whole chunk, the bare two-space
        // continuation indent otherwise: a narrow panel keeps whole chunks, not a label.
        let indent = if room >= 9 + URL_MIN { 9 } else { 2 };
        let per = room.saturating_sub(indent);
        if per >= URL_MIN {
            for (i, chunk) in url.chars().collect::<Vec<_>>().chunks(per).enumerate() {
                let text: String = chunk.iter().collect();
                let row = if i == 0 && indent == 9 {
                    field("url", p.accent.fg(&text))
                } else {
                    format!("{}{}", " ".repeat(indent), p.accent.fg(&text))
                };
                out.push(Row::Line(row));
            }
        } else {
            // Too narrow to break into whole chunks: the head of the URL, cut where a row
            // ends, says more than a first chunk the panel would cut anyway.
            let label = if room >= 9 + 4 { 9 } else { 0 };
            let text = truncate_to_width(url, room.saturating_sub(label), glyph("…"));
            out.push(Row::Line(if label > 0 {
                field("url", p.accent.fg(&text))
            } else {
                p.accent.fg(&text)
            }));
        }
    }
    out.push(Row::Line(p.text.fg(phase_text(&status))));
    // The last few lines the provider printed: enough to see a prompt or a complaint. The child
    // is a pty, so a progress bar arrives as `\r` redraws and its control bytes would move the
    // terminal's cursor inside a drawn row; the transcript's own cleaning makes them safe.
    let tail = status.lines.len().saturating_sub(LAST_OUTPUT);
    for line in &status.lines[tail..] {
        out.push(Row::Line(p.muted.fg(&format!("  {}", one_line(line)))));
    }
    if o.login.needs_input() {
        out.push(Row::Line(String::new()));
        out.push(Row::Line(
            p.muted.fg("paste the code here, then press enter"),
        ));
        // One bullet per character: the token itself is never drawn, so it cannot end up in
        // a terminal scrollback or a screenshot.
        out.push(Row::Line(format!(
            "{} {}{CURSOR_MARKER}",
            p.accent.fg(glyph("›")),
            p.text.fg(&o.login.masked())
        )));
    }
    out
}

/// One line of the sign-in's output as a panel row can hold it: cleaned like the transcript's
/// text, then only the last redraw, since a row is one line and a pty's `\r` means "overwrite".
fn one_line(text: &str) -> String {
    let text = clean(text);
    let last = text.rsplit('\n').find(|last| !last.is_empty());
    last.unwrap_or_default().to_string()
}

/// What the sign-in is waiting for, in one line. `done` and `failed` are drawn on the tick
/// before the panel closes, so they must not claim to be waiting for anything.
fn phase_text(status: &LoginStatus) -> &'static str {
    match status.phase.as_str() {
        "need_input" => "waiting for the code it asks for",
        "done" => "signed in",
        "failed" => "the sign-in failed",
        _ => "waiting for you to finish in the browser",
    }
}

/// The vault panel: its entries and what the keys do, or the form, or the delete question.
fn vault_rows(o: &Overlay, p: &Palette) -> Vec<Row> {
    let v = &o.vault;
    let mut out = Vec::new();
    if let Some(problem) = &v.problem {
        out.push(Row::Line(p.error.fg(problem)));
    }
    if let Some(form) = &v.form {
        let what = match form.kind {
            crate::vault::Kind::Login => "login",
            crate::vault::Kind::Token => "token",
        };
        let title = match form.editing {
            true => format!("edit {what} {}", form.get("name")),
            false => format!("add a {what}"),
        };
        out.push(Row::Head(title, String::new()));
        for (i, (label, secret, text)) in form.fields.iter().enumerate() {
            let shown = match secret {
                true => glyph("•").repeat(text.chars().count()),
                false => text.clone(),
            };
            let (mark, cursor) = match i == form.at {
                true => (p.accent.fg(glyph("›")), CURSOR_MARKER),
                false => (" ".into(), ""),
            };
            let label = p.muted.fg(&format!("{label:<10}"));
            out.push(Row::Line(format!(
                " {mark} {label}{}{cursor}",
                p.text.fg(&shown)
            )));
        }
        let keep = match form.editing {
            true => "; an empty secret keeps the saved one",
            false => "",
        };
        out.push(Row::Line(p.muted.fg(&format!(
            "tab next field, enter on the last saves, esc back{keep}"
        ))));
        return out;
    }
    if let Some(name) = &v.confirm {
        out.push(Row::Line(p.text.fg(&format!(
            "delete {name}? y deletes it, any other key keeps it"
        ))));
        return out;
    }
    let Some(entries) = &v.entries else {
        out.push(Row::Line(p.muted.fg(&format!("listing{}", glyph("…")))));
        return out;
    };
    if entries.is_empty() {
        out.push(Row::Line(p.muted.fg("nothing saved yet")));
    }
    for e in entries {
        let note = match e.kind.as_str() {
            "token" => match e.programs.as_slice() {
                [] => "token, no program allowed yet".to_string(),
                progs => format!("token, allowed for {}", progs.join(", ")),
            },
            _ => {
                let user = match e.username.as_str() {
                    "" => String::new(),
                    u => format!("{u} on "),
                };
                let code = if e.totp { ", one-time code" } else { "" };
                let grant = if e.granted {
                    "filling allowed"
                } else {
                    "asks first"
                };
                format!("{user}{}{code}; {grant}", e.site)
            }
        };
        out.push(Row::Item {
            pick: e.name.clone(),
            label: e.name.clone(),
            note,
            current: false,
        });
    }
    out.push(Row::Line(p.muted.fg(
        "a add login, t add token, e edit, d delete, r revoke grants, esc close",
    )));
    out
}

/// The vault panel's keys. The form takes every printable key while it is open.
fn vault_key(o: &mut Overlay, k: KeyEvent, rows: &[Row]) -> Action {
    let v = &mut o.vault;
    if let Some(form) = &mut v.form {
        let last = form.fields.len() - 1;
        match k.code {
            KeyCode::Esc => v.form = None,
            KeyCode::Tab | KeyCode::Down => form.at = (form.at + 1).min(last),
            KeyCode::BackTab | KeyCode::Up => form.at = form.at.saturating_sub(1),
            KeyCode::Enter if form.at < last => form.at += 1,
            KeyCode::Enter => {
                let form = v.form.take().expect("open");
                v.problem = None;
                return Action::VaultPut(Box::new(form.entry()));
            }
            KeyCode::Backspace if !(form.editing && form.at == 0) => {
                form.fields[form.at].2.pop();
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                form.type_text(&c.to_string())
            }
            _ => {}
        }
        return Action::None;
    }
    if let Some(name) = v.confirm.take() {
        return match k.code {
            KeyCode::Char('y') => Action::VaultRm(name),
            _ => Action::None,
        };
    }
    let picks = picks(rows);
    let chosen = picks.get(o.selected).map(|&i| &rows[i]);
    let name = match chosen {
        Some(Row::Item { pick, .. }) => Some(pick.clone()),
        _ => None,
    };
    let entry = || {
        let list = o.vault.entries.iter().flatten();
        list.clone()
            .find(|e| Some(&e.name) == name.as_ref())
            .cloned()
    };
    match k.code {
        KeyCode::Esc => return Action::Close,
        KeyCode::Up => o.selected = o.selected.saturating_sub(1),
        KeyCode::Down => o.selected = (o.selected + 1).min(picks.len().saturating_sub(1)),
        KeyCode::Char('a') => {
            o.vault.form = Some(Form::new(crate::vault::Kind::Login, ["", "", ""]))
        }
        KeyCode::Char('t') => {
            o.vault.form = Some(Form::new(crate::vault::Kind::Token, ["", "", ""]))
        }
        KeyCode::Char('e') => o.vault.form = entry().as_ref().map(Form::edit),
        KeyCode::Char('d') => o.vault.confirm = name,
        KeyCode::Char('r') => {
            if let Some(name) = name {
                return Action::VaultRevoke(name);
            }
        }
        _ => {}
    }
    Action::None
}

/// A paste into the vault form goes to the field at the cursor, as typing would.
pub fn paste(o: &mut Overlay, text: &str) {
    if let Some(form) = o.vault.form.as_mut().filter(|_| o.kind == Kind::Vault) {
        form.type_text(text);
    }
}

/// The login picker's keys: pick a provider, then type the token it asks for. Everything else
/// scrolls, because a sign-in's output is the only thing to read while it runs.
fn login_key(o: &mut Overlay, k: KeyEvent, rows: &[Row]) -> Action {
    if k.code == KeyCode::Esc {
        return match o.login.running() {
            Some(id) => Action::Cancel(id.to_string()),
            None => Action::Close,
        };
    }
    if o.login.needs_input() {
        // The keyboard belongs to the token: no filtering, no selection, no picker under it.
        return match k.code {
            KeyCode::Enter => match o.login.take() {
                text if text.trim().is_empty() => Action::None,
                text => Action::Input(text),
            },
            KeyCode::Backspace => {
                o.login.backspace();
                Action::None
            }
            KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
                o.login.type_char(c);
                Action::None
            }
            _ => Action::None,
        };
    }
    let picks = picks(rows);
    let last = picks.len().saturating_sub(1);
    let scrolling = picks.is_empty();
    match k.code {
        KeyCode::Enter if !scrolling => {
            let Some(&i) = picks.get(o.selected) else {
                return Action::None;
            };
            let Row::Item { pick, .. } = &rows[i] else {
                return Action::None;
            };
            return Action::Login(pick.clone());
        }
        KeyCode::Up if scrolling => o.top = o.top.saturating_sub(1),
        KeyCode::Down if scrolling => o.top += 1,
        KeyCode::PageUp if scrolling => o.top = o.top.saturating_sub(o.page.max(1)),
        KeyCode::PageDown if scrolling => o.top += o.page.max(1),
        KeyCode::Up => o.selected = o.selected.saturating_sub(1),
        KeyCode::Down => o.selected = (o.selected + 1).min(last),
        KeyCode::Home => o.selected = 0,
        KeyCode::End => o.selected = last,
        KeyCode::Backspace if !scrolling => {
            o.query.pop();
            o.selected = 0;
        }
        KeyCode::Char(c) if !scrolling && !k.modifiers.contains(KeyModifiers::CONTROL) => {
            o.query.push(c);
            o.selected = 0;
        }
        _ => {}
    }
    Action::None
}

/// Whether the panel draws the filter line: every panel that lists something, and the login
/// picker only until its sign-in starts — the token goes where the filter was.
fn lists(o: &Overlay) -> bool {
    match o.kind {
        Kind::Status | Kind::Vault => false,
        Kind::Login => o.login.running().is_none(),
        Kind::Model | Kind::Help => true,
    }
}

/// The positions of the pickable rows.
fn picks(rows: &[Row]) -> Vec<usize> {
    (0..rows.len())
        .filter(|&i| matches!(rows[i], Row::Item { .. }))
        .collect()
}

/// Moves the selection, scrolls, filters, or picks.
pub fn key(o: &mut Overlay, k: KeyEvent, rows: &[Row]) -> Action {
    if o.kind == Kind::Login {
        return login_key(o, k, rows);
    }
    if o.kind == Kind::Vault {
        return vault_key(o, k, rows);
    }
    let picks = picks(rows);
    let last = picks.len().saturating_sub(1);
    let page = o.page.max(1);
    let scrolls = picks.is_empty();
    match k.code {
        KeyCode::Esc => return Action::Close,
        KeyCode::Enter => {
            return match picks.get(o.selected).map(|&i| &rows[i]) {
                Some(Row::Item { pick, .. }) => Action::Pick(pick.clone()),
                _ => Action::None,
            }
        }
        KeyCode::Up if scrolls => o.top = o.top.saturating_sub(1),
        KeyCode::Down if scrolls => o.top += 1,
        KeyCode::PageUp if scrolls => o.top = o.top.saturating_sub(page),
        KeyCode::PageDown if scrolls => o.top += page,
        KeyCode::Up => o.selected = o.selected.saturating_sub(1),
        KeyCode::Down => o.selected = (o.selected + 1).min(last),
        KeyCode::PageUp => o.selected = o.selected.saturating_sub(page),
        KeyCode::PageDown => o.selected = (o.selected + page).min(last),
        KeyCode::Home => o.selected = 0,
        KeyCode::End => o.selected = last,
        KeyCode::Backspace if o.kind != Kind::Status => {
            o.query.pop();
            o.selected = 0;
        }
        KeyCode::Char(c)
            if o.kind != Kind::Status && !k.modifiers.contains(KeyModifiers::CONTROL) =>
        {
            o.query.push(c);
            o.selected = 0;
        }
        _ => {}
    }
    Action::None
}

/// The panel: a rounded box titled like the reply cards, a filter line for the lists, then as
/// many rows as fit in `height`, scrolled to keep the selection in view.
pub fn render(
    o: &mut Overlay,
    rows: &[Row],
    p: &Palette,
    width: usize,
    height: usize,
) -> Vec<String> {
    let inner = width.saturating_sub(4).max(1);
    let h = glyph("─");
    let (tl, tr, bl, br, v) = if h == "─" {
        ("╭", "╮", "╰", "╯", "│")
    } else {
        ("+", "+", "+", "+", "|")
    };
    let title = match o.kind {
        Kind::Model => " model ",
        Kind::Status => " status ",
        Kind::Help => " commands ",
        Kind::Login => " sign in ",
        Kind::Vault => " vault ",
    };
    let fill = width.saturating_sub(visible_width(title) + 3);
    let mut lines = vec![format!(
        "{}{}{}",
        p.dim.fg(&format!("{tl}{h}")),
        p.muted.fg(&bold(title)),
        p.dim.fg(&format!("{}{tr}", h.repeat(fill)))
    )];
    let boxed = |content: &str, bg: bool| {
        let content = truncate_to_width(content, inner, glyph("…"));
        let pad = inner.saturating_sub(visible_width(&content.replace(CURSOR_MARKER, "")));
        let body = format!("{content}{}", " ".repeat(pad));
        let body = if bg { p.surface.bg(&body) } else { body };
        format!("{} {body} {}", p.dim.fg(v), p.dim.fg(v))
    };
    if lists(o) {
        let prompt = p.accent.fg(glyph("›"));
        let filter = match o.query.is_empty() {
            true => format!("{prompt} {CURSOR_MARKER}{}", p.muted.fg("type to filter")),
            false => format!("{prompt} {}{CURSOR_MARKER}", p.text.fg(&o.query)),
        };
        lines.push(boxed(&filter, false));
    }

    let picks = picks(rows);
    if !o.placed && !picks.is_empty() {
        let current = picks
            .iter()
            .position(|&i| matches!(rows[i], Row::Item { current: true, .. }));
        o.selected = current.unwrap_or(0);
        o.placed = o.models.is_some() || o.kind != Kind::Model;
    }
    // A view with nothing to pick (the vault's form or delete question) keeps the selection for
    // the list it goes back to.
    if !picks.is_empty() {
        o.selected = o.selected.min(picks.len() - 1);
    }
    let cap = height.saturating_sub(lines.len() + 1).max(1);
    o.page = cap;
    if let Some(&at) = picks.get(o.selected) {
        // A group's first item brings its heading into view.
        let at_head =
            at.saturating_sub(usize::from(at > 0 && matches!(rows[at - 1], Row::Head(..))));
        o.top = o.top.min(at_head).max((at + 1).saturating_sub(cap));
    }
    o.top = o.top.min(rows.len().saturating_sub(cap));

    let label_w = column((rows.iter()).filter_map(|r| match r {
        Row::Item { label, .. } => Some(label.as_str()),
        _ => None,
    }));
    let chosen = picks.get(o.selected).copied();
    for (i, row) in rows.iter().enumerate().skip(o.top).take(cap) {
        let sel = chosen == Some(i);
        let line = match row {
            Row::Head(name, note) => format!("{}  {}", p.text.fg(&bold(name)), p.muted.fg(note)),
            Row::Item {
                label,
                note,
                current,
                ..
            } => {
                let mark = if sel {
                    p.accent.fg(glyph("›"))
                } else {
                    " ".into()
                };
                // Only the model picker has a current choice to mark.
                let dot = match (o.kind, current) {
                    (Kind::Model, true) => format!("{} ", p.accent.fg(glyph("●"))),
                    (Kind::Model, false) => "  ".into(),
                    _ => String::new(),
                };
                let label = format!(
                    "{label}{}",
                    " ".repeat(label_w.saturating_sub(visible_width(label)))
                );
                let label = if sel { bold(&label) } else { label };
                format!(" {mark} {dot}{}  {}", p.text.fg(&label), p.muted.fg(note))
            }
            Row::Line(text) => text.clone(),
        };
        lines.push(boxed(&line, sel));
    }

    let more = if rows.len() > cap {
        let end = (o.top + cap).min(rows.len());
        format!(" {}-{end} of {} ", o.top + 1, rows.len())
    } else {
        String::new()
    };
    let fill = width.saturating_sub(visible_width(&more) + 3);
    lines.push(format!(
        "{}{}{}",
        p.dim.fg(&format!("{bl}{}", h.repeat(fill))),
        p.muted.fg(&more),
        p.dim.fg(&format!("{h}{br}"))
    ));
    lines
}

/// The width of a label column: the widest label that is not an outlier; a longer one
/// pushes its note right instead of widening every row.
fn column<'a>(labels: impl Iterator<Item = &'a str>) -> usize {
    const MAX: usize = 26;
    labels
        .map(visible_width)
        .filter(|&w| w <= MAX)
        .max()
        .unwrap_or(0)
}

/// Slash autocomplete: the commands the composer's text could still become.
#[derive(Debug, Clone, PartialEq)]
pub struct Complete {
    /// Indexes into [`COMMANDS`].
    pub items: Vec<usize>,
    pub selected: usize,
}

/// The commands `text` is the start of, or whose arguments it is typing.
pub fn matches(text: &str) -> Vec<usize> {
    if !text.starts_with('/') || text.contains('\n') {
        return vec![];
    }
    (0..COMMANDS.len())
        .filter(|&i| {
            let ins = insert(COMMANDS[i].1);
            ins.starts_with(text) || (ins.ends_with(' ') && text.starts_with(ins))
        })
        .collect()
}

/// The rows above the composer: at most six, scrolled to the selection.
pub fn completions(s: &State, p: &Palette, width: usize, _now: i64) -> Vec<String> {
    const ROWS: usize = 6;
    let Some(c) = &s.complete else { return vec![] };
    let top = (c.selected + 1).saturating_sub(ROWS);
    let shown: Vec<(usize, usize)> = c
        .items
        .iter()
        .copied()
        .enumerate()
        .skip(top)
        .take(ROWS)
        .collect();
    let w = column(shown.iter().map(|&(_, i)| COMMANDS[i].1));
    shown
        .into_iter()
        .map(|(k, i)| {
            let (_, usage, what) = COMMANDS[i];
            let sel = k == c.selected;
            let mark = if sel {
                p.accent.fg(glyph("›"))
            } else {
                " ".into()
            };
            let label = format!(
                "{usage}{}",
                " ".repeat(w.saturating_sub(visible_width(usage)))
            );
            let label = if sel { bold(&label) } else { label };
            let row = format!(" {mark} {}  {}", p.text.fg(&label), p.muted.fg(what));
            truncate_to_width(&row, width, glyph("…"))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::super::theme::DARK;
    use super::*;
    use crate::router::{AccountState, NextJob};
    use crossterm::event::KeyEvent;

    /// 2026-10-30 14:02 UTC, in milliseconds.
    const NOW: i64 = 1_793_368_920_000;

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|l| {
                rpi_tui::strip_ansi(&l.replace(CURSOR_MARKER, ""))
                    .trim_end()
                    .to_string()
            })
            .collect()
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn snap() -> Snapshot {
        let account = |name: &str, model: &str, state: &str, until: Option<i64>| AccountState {
            name: name.into(),
            model: model.into(),
            state: state.into(),
            until_unix: until,
        };
        Snapshot {
            account: Some("work".into()),
            model: Some("anthropic/claude-opus-5".into()),
            thinking: Some("medium".into()),
            accounts: vec![
                account("work", "anthropic/claude-opus-5", "ready", None),
                account("spare", "openai/gpt-6", "cooling", Some(NOW / 1000 + 3600)),
            ],
            jobs: vec![NextJob {
                id: 3,
                name: "renew domain".into(),
                next_unix: NOW / 1000 + 86_400,
            }],
            ..Default::default()
        }
    }

    fn models() -> Vec<AccountModels> {
        vec![
            AccountModels {
                name: Some("work".into()),
                model: Some("anthropic/claude-opus-5".into()),
                models: vec![
                    "anthropic/claude-opus-5".into(),
                    "anthropic/claude-sonnet-5".into(),
                ],
            },
            AccountModels {
                name: Some("spare".into()),
                model: Some("openai/gpt-6".into()),
                models: vec!["openai/gpt-6".into()],
            },
        ]
    }

    fn list(o: &Overlay, s: &Snapshot) -> Vec<Row> {
        rows(o, s, &DARK, &jiff::tz::TimeZone::UTC, NOW, 80)
    }

    fn draw(o: &mut Overlay, s: &Snapshot, width: usize, height: usize) -> Vec<String> {
        let rows = list(o, s);
        plain(&render(o, &rows, &DARK, width, height))
    }

    /// The panel's plain lines at another width, for the wrapping test.
    fn lines_at(o: &Overlay, s: &Snapshot, width: usize) -> Vec<String> {
        let rows = rows(o, s, &DARK, &jiff::tz::TimeZone::UTC, NOW, width);
        let lines: Vec<String> = rows
            .into_iter()
            .filter_map(|r| match r {
                Row::Line(t) => Some(t),
                _ => None,
            })
            .collect();
        plain(&lines)
    }

    fn hit(o: &mut Overlay, s: &Snapshot, code: KeyCode) -> Action {
        let rows = list(o, s);
        key(o, press(code), &rows)
    }

    #[test]
    fn the_model_picker_groups_by_account_and_sends_the_pick() {
        let s = snap();
        let mut o = Overlay::new(Kind::Model);
        // Before zephyr.models answers: each account's own model.
        let early = draw(&mut o, &s, 60, 20);
        assert_eq!(
            early,
            [
                "╭─ model ──────────────────────────────────────────────────╮",
                "│ › type to filter                                         │",
                "│ work  in use                                             │",
                "│  › ● claude-opus-5                                       │",
                "│ spare  cooling until 15:02                               │",
                "│      gpt-6                                               │",
                "│ listing models…                                          │",
                "╰──────────────────────────────────────────────────────────╯",
            ]
        );
        o.models = Some(models());
        o.placed = false;
        let got = draw(&mut o, &s, 60, 20);
        assert_eq!(
            got[3],
            "│  › ● claude-opus-5                                       │"
        );
        assert_eq!(
            got[4],
            "│      claude-sonnet-5                                     │"
        );
        // Down moves past the heading to the next account's model; Enter picks it.
        assert_eq!(hit(&mut o, &s, KeyCode::Down), Action::None);
        assert_eq!(hit(&mut o, &s, KeyCode::Down), Action::None);
        let pick = hit(&mut o, &s, KeyCode::Enter);
        assert_eq!(pick, Action::Pick("/model openai/gpt-6".into()));
        // Typing filters across accounts; Esc closes.
        for c in "sonn".chars() {
            hit(&mut o, &s, KeyCode::Char(c));
        }
        let got = draw(&mut o, &s, 60, 20);
        assert_eq!(
            got[1],
            "│ › sonn                                                   │"
        );
        assert_eq!(
            got[2..4],
            [
                "│ work  in use                                             │",
                "│  ›   claude-sonnet-5                                     │"
            ]
        );
        assert_eq!(got.len(), 5);
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn a_long_list_scrolls_with_the_selection() {
        let mut o = Overlay::new(Kind::Help);
        let rows = list(&o, &Snapshot::default());
        let got = plain(&render(&mut o, &rows, &DARK, 80, 8));
        assert_eq!(got.len(), 8);
        assert_eq!(got[2], format!("│ Chat{}│", " ".repeat(73)));
        assert!(got[7].ends_with(" 1-5 of 66 ─╯"), "{got:?}");
        for _ in 0..27 {
            key(&mut o, press(KeyCode::Down), &rows);
        }
        let got = plain(&render(&mut o, &rows, &DARK, 80, 8));
        // The twenty-eighth command is /memory ok; the window follows it down.
        assert_eq!(
            got[6],
            "│  › /memory ok                  let the agent see the waiting change          │"
        );
        assert!(got[7].ends_with(" 27-31 of 66 ─╯"), "{got:?}");
        key(&mut o, press(KeyCode::Down), &rows);
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick("/memory undo".into())
        );
        // The keys come last; the TUI inserts none of them.
        key(&mut o, press(KeyCode::End), &rows);
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick("double-click".into())
        );
    }

    #[test]
    fn help_lists_every_command_in_its_group() {
        let o = Overlay::new(Kind::Help);
        let heads: Vec<String> = (list(&o, &Snapshot::default()).into_iter())
            .filter_map(|r| match r {
                Row::Head(h, _) => Some(h),
                _ => None,
            })
            .collect();
        assert_eq!(
            heads,
            ["Chat", "Agent", "Memory", "Schedule", "Vault", "Gateway", "Keys"]
        );
        let mut o = Overlay::new(Kind::Help);
        o.query = "rule".into();
        let picks: Vec<String> = (list(&o, &Snapshot::default()).into_iter())
            .filter_map(|r| match r {
                Row::Item { pick, .. } => Some(pick),
                _ => None,
            })
            .collect();
        // `/new` says memory and rules carry over.
        assert_eq!(picks, ["/new", "/rules", "/rule ", "/rule rm "]);
    }

    /// The memory, skill, connector and job commands in the list are the ones those modules answer.
    #[test]
    fn listed_commands_are_the_ones_zephyr_answers() {
        let dir = crate::test_dir("tui-commands");
        let mut store = crate::store::Store::open(&dir.join("state.db")).unwrap();
        let cfg: crate::config::Config = toml::from_str("rpi = 'rpi'\nhome = 'h'").unwrap();
        for (group, usage, _) in COMMANDS {
            let text = usage
                .replace("<n>", "1")
                .replace("<id>", "1")
                .replace("<text>", "x");
            let answered = match *group {
                "Memory" => {
                    crate::memory::command(&dir, &dir, &text).is_some()
                        || crate::drafts::command(&cfg, &text).is_some()
                }
                "Schedule" => {
                    let m = crate::channel::Inbound {
                        channel: "tui".into(),
                        platform_msg_id: "1".into(),
                        sender: "me".into(),
                        text: text.clone(),
                    };
                    crate::scheduler::command(&mut store, &cfg, &m)
                        .is_some_and(|r| !r.contains("usage:"))
                }
                _ => continue,
            };
            assert!(answered, "{text}");
        }
    }

    #[test]
    fn the_vault_panel_lists_entries_and_its_form_never_shows_a_secret() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Vault);
        assert!(draw(&mut o, &s, 80, 12)
            .iter()
            .any(|l| l.contains("listing")));
        o.vault.entries = Some(vec![
            VaultEntry {
                kind: "login".into(),
                name: "github".into(),
                site: "https://github.com".into(),
                username: "aiden".into(),
                totp: true,
                granted: true,
                programs: vec![],
            },
            VaultEntry {
                kind: "token".into(),
                name: "GH_TOKEN".into(),
                programs: vec!["gh".into()],
                ..Default::default()
            },
        ]);
        let got = draw(&mut o, &s, 90, 12);
        assert!(
            got.iter().any(|l| l
                .contains("github    aiden on https://github.com, one-time code; filling allowed")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|l| l.contains("GH_TOKEN  token, allowed for gh")),
            "{got:?}"
        );

        // e edits the selected entry: its name is fixed, the cursor on the password.
        assert_eq!(hit(&mut o, &s, KeyCode::Char('e')), Action::None);
        let form = o.vault.form.clone().unwrap();
        assert!(form.editing && form.fields[form.at].0 == "password");
        for c in "hunter2".chars() {
            hit(&mut o, &s, KeyCode::Char(c));
        }
        paste(&mut o, "!x");
        let got = draw(&mut o, &s, 90, 12);
        assert!(
            got.iter().any(|l| l.contains("password  •••••••••")),
            "{got:?}"
        );
        assert!(!got.join("\n").contains("hunter2"), "{got:?}");
        assert!(!format!("{o:?}").contains("hunter2"));
        hit(&mut o, &s, KeyCode::Tab);
        let Action::VaultPut(entry) = hit(&mut o, &s, KeyCode::Enter) else {
            panic!("no save");
        };
        assert_eq!(
            (entry.name.as_str(), entry.password.as_str()),
            ("github", "hunter2!x")
        );
        assert!(
            o.vault.form.is_none(),
            "the typed secret leaves with the save"
        );

        // d asks first; only y deletes. r revokes the selected entry's grants.
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(hit(&mut o, &s, KeyCode::Char('d')), Action::None);
        assert!(draw(&mut o, &s, 90, 12)
            .iter()
            .any(|l| l.contains("delete GH_TOKEN?")));
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('y')),
            Action::VaultRm("GH_TOKEN".into())
        );
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('r')),
            Action::VaultRevoke("GH_TOKEN".into())
        );
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn vault_add_commands_open_a_filled_in_form() {
        let f = Form::from_command("/vault add login github https://github.com").unwrap();
        assert_eq!(f.get("name"), "github");
        assert_eq!(f.get("site"), "https://github.com");
        assert_eq!(f.fields[f.at].0, "username");
        let t = Form::from_command("/vault add token GH_TOKEN").unwrap();
        assert_eq!(
            (t.kind, t.fields[t.at].0),
            (crate::vault::Kind::Token, "value")
        );
        assert!(Form::from_command("/vault add token GH_TOKEN ghp_pasted").is_none());
        assert!(Form::from_command("/vault rm x").is_none());
    }

    #[test]
    fn the_status_panel_reads_the_snapshot() {
        let mut s = snap();
        s.failed_replies = 2;
        s.paused = Some("user".into());
        let mut o = Overlay::new(Kind::Status);
        let got = draw(&mut o, &s, 80, 30);
        let want = [
            "╭─ status ─────────────────────────────────────────────────────────────────────╮",
            "│ agent     work › claude-opus-5 · thinking medium                             │",
            "│           idle                                                               │",
            "│ accounts  ● work   claude-opus-5  in use                                     │",
            "│           ◐ spare  gpt-6          cooling until 15:02                        │",
            "│ channels                                                                     │",
            "│ gateway   ⏸ paused · new messages wait until /resume                         │",
            "│ spend     today 0 tok · $0.00                                                │",
            "│ queue     empty                                                              │",
            "│ replies   ✗ 2 failed replies                                                 │",
            "│ jobs      Sat 14:02  renew domain #3                                         │",
            "╰──────────────────────────────────────────────────────────────────────────────╯",
        ];
        assert_eq!(got, want);
    }

    #[test]
    fn autocomplete_matches_commands_and_their_arguments() {
        let usages =
            |t: &str| -> Vec<&str> { matches(t).into_iter().map(|i| COMMANDS[i].1).collect() };
        // All but `!<command>` and the keys, which are not slash commands.
        let keys = COMMANDS.iter().filter(|c| c.0 == "Keys").count();
        assert_eq!(usages("/").len(), COMMANDS.len() - 1 - keys);
        assert!(usages("!").is_empty());
        assert_eq!(usages("/mo"), ["/model", "/model <provider/model>"]);
        assert_eq!(usages("/rule"), ["/rules", "/rule <text>", "/rule rm <n>"]);
        assert_eq!(usages("/rule never push"), ["/rule <text>"]);
        assert_eq!(usages("/rules"), ["/rules"]);
        assert!(usages("hello").is_empty() && usages("/x").is_empty());
        assert!(usages("/status\nmore").is_empty());
        assert_eq!(
            insert("/job add <name> | <cron or time> | <prompt>"),
            "/job add "
        );

        let s = State {
            complete: Some(Complete {
                items: matches("/rule"),
                selected: 1,
            }),
            ..Default::default()
        };
        let got: Vec<String> = completions(&s, &DARK, 60, 0)
            .iter()
            .map(|l| rpi_tui::strip_ansi(l).trim_end().to_string())
            .collect();
        assert_eq!(
            got,
            [
                "   /rules        your standing rules",
                " › /rule <text>  add a rule the agent reads first",
                "   /rule rm <n>  remove rule n",
            ]
        );
    }

    /// Two of the providers the daemon offers, in its order.
    fn providers() -> Vec<Provider> {
        vec![
            Provider {
                id: "opencode-go".into(),
                label: "OpenCode Go".into(),
                kind: "key".into(),
            },
            Provider {
                id: "claude-code".into(),
                label: "Claude (Agent SDK)".into(),
                kind: "setup_token".into(),
            },
        ]
    }

    fn claude_login() -> Login {
        Login {
            id: "l1".into(),
            provider: "claude-code".into(),
            account: None,
            kind: "setup_token".into(),
        }
    }

    fn waiting(phase: &str) -> LoginStatus {
        LoginStatus {
            phase: phase.into(),
            url: Some("https://claude.ai/oauth/authorize?code=x".into()),
            lines: vec!["Paste code here if prompted >".into()],
            ..Default::default()
        }
    }

    #[test]
    fn the_login_picker_lists_what_the_daemon_offers_to_sign_in_to() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        // Before zephyr.login_providers answers.
        let early = draw(&mut o, &s, 62, 12);
        assert!(early[2].contains("listing providers"), "{early:?}");
        o.login.providers = Some(providers());
        let got = draw(&mut o, &s, 62, 12);
        assert!(
            got[2].starts_with("│ sign in  a new account joins the pool"),
            "{got:?}"
        );
        // The provider's id starts it; the note says what signing in takes.
        assert!(got[3].contains("OpenCode Go"), "{got:?}");
        assert!(got[3].contains("paste an API key"), "{got:?}");
        assert!(got[4].contains("Claude (Agent SDK)"), "{got:?}");
        assert!(got[4].contains("paste a setup-token code"), "{got:?}");
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::Login("opencode-go".into())
        );
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::Login("claude-code".into())
        );
        // The filter reads the note too, so "setup" finds the provider that asks for a code.
        for c in "setup".chars() {
            hit(&mut o, &s, KeyCode::Char(c));
        }
        let got = draw(&mut o, &s, 62, 12);
        assert!(got[3].contains("Claude (Agent SDK)"), "{got:?}");
        assert!(!got.iter().any(|l| l.contains("OpenCode")), "{got:?}");
        for _ in 0..5 {
            hit(&mut o, &s, KeyCode::Backspace);
        }
        for c in "zzz".chars() {
            hit(&mut o, &s, KeyCode::Char(c));
        }
        let got = draw(&mut o, &s, 62, 12);
        assert!(got[3].contains("nothing matches"), "{got:?}");
        // Esc on the list closes the panel; there is no sign-in to give up on yet.
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn a_running_login_shows_its_url_and_masks_the_token_it_asks_for() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        o.login.providers = Some(providers());
        o.login.session = Some(claude_login());
        // login_start's answer arrives before the first poll: no status yet, still running.
        assert_eq!(o.login.running(), Some("l1"));
        let early = draw(&mut o, &s, 80, 20);
        assert!(
            early
                .iter()
                .any(|l| l.contains("waiting for you to finish in the browser")),
            "{early:?}"
        );
        o.login.status = Some(waiting("need_input"));
        let got = draw(&mut o, &s, 80, 20);
        // No filter line while it runs: the keyboard belongs to the token now.
        assert!(got[1].contains("provider"), "{got:?}");
        assert!(got[1].contains("Claude (Agent SDK)"), "{got:?}");
        assert!(got[2].contains("account"), "{got:?}");
        assert!(
            got.iter()
                .any(|l| l.contains("https://claude.ai/oauth/authorize?code=x")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|l| l.contains("waiting for the code it asks for")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|l| l.contains("Paste code here if prompted")),
            "{got:?}"
        );
        // One bullet per character: the token itself is drawn nowhere.
        for c in "sk-ant-oat".chars() {
            hit(&mut o, &s, KeyCode::Char(c));
        }
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter().any(|l| l.contains(&glyph("•").repeat(10))),
            "{got:?}"
        );
        assert!(!got.iter().any(|l| l.contains("sk-ant")), "{got:?}");
        // Enter takes it once, and the panel forgets it in the same breath.
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::Input("sk-ant-oat".into())
        );
        assert_eq!(o.login.masked(), "");
        assert!(!format!("{o:?}").contains("sk-ant"), "{o:?}");
        let got = draw(&mut o, &s, 80, 20);
        assert!(!got.iter().any(|l| l.contains(glyph("•"))), "{got:?}");
        // A second Enter sends nothing rather than an empty line.
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        // Esc gives the sign-in up, and the panel goes with it: mod.rs sends the cancel and
        // closes, rather than dropping back to a list behind a login that is already gone.
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::Cancel("l1".into()));
    }

    #[test]
    fn a_control_byte_from_the_login_process_never_lands_in_a_row() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        o.login.session = Some(claude_login());
        // A pty redraws its progress bar with `\r`, and so may send other control bytes.
        o.login.status = Some(LoginStatus {
            phase: "waiting".into(),
            lines: vec!["10%\r20%\u{7}\u{1b}[31mx".into(), "tab\there".into()],
            ..Default::default()
        });
        let text: Vec<String> = (list(&o, &s).into_iter())
            .filter_map(|r| match r {
                Row::Line(t) => Some(rpi_tui::strip_ansi(&t)),
                _ => None,
            })
            .collect();
        // The last redraw of the bar is what it said last, and nothing else from it.
        assert!(text.iter().any(|l| l.contains("  20%x")), "{text:?}");
        assert!(text.iter().any(|l| l.contains("tab    here")), "{text:?}");
        assert!(
            !text
                .iter()
                .any(|l| l.contains('\r') || l.contains('\u{7}') || l.contains("10%")),
            "{text:?}"
        );
    }

    #[test]
    fn a_finished_or_failed_snapshot_never_claims_to_be_waiting() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        o.login.session = Some(claude_login());
        for (phase, said) in [("failed", "the sign-in failed"), ("done", "signed in")] {
            o.login.status = Some(LoginStatus {
                phase: phase.into(),
                ..Default::default()
            });
            let got = draw(&mut o, &s, 80, 20);
            assert!(got.iter().any(|l| l.contains(said)), "{got:?}");
            assert!(!got.iter().any(|l| l.contains("browser")), "{got:?}");
        }
    }

    #[test]
    fn a_browser_login_has_no_token_row_and_the_list_says_when_it_cannot_sign_in() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        o.login.session = Some(Login {
            id: "l2".into(),
            provider: "opencode-go".into(),
            account: Some("go-2".into()),
            kind: "oauth".into(),
        });
        o.login.status = Some(waiting("waiting"));
        let got = draw(&mut o, &s, 80, 20);
        // The provider is named by id when the list never arrived.
        assert!(got[1].contains("opencode-go"), "{got:?}");
        // The daemon's account name, not "the next free name", once it has one.
        assert!(got[2].contains("go-2"), "{got:?}");
        assert!(!got.iter().any(|l| l.contains(glyph("•"))), "{got:?}");
        assert_eq!(hit(&mut o, &s, KeyCode::Char('x')), Action::None);

        // A daemon without the method answers an error, which leaves an empty list.
        let mut o = Overlay::new(Kind::Login);
        o.login.providers = Some(Vec::new());
        let got = draw(&mut o, &s, 80, 12);
        assert!(got[2].contains("cannot sign anyone in"), "{got:?}");
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
    }

    #[test]
    fn a_start_is_adopted_only_by_the_panel_that_asked_and_only_once() {
        let mut o = Overlay::new(Kind::Login);
        o.login.providers = Some(providers());
        assert!(o.login.ask("claude-code"));
        // A second Enter while the first start is unanswered opens nothing: one start at a
        // time, so an answer can only be the one the panel is waiting for.
        assert!(!o.login.ask("opencode-go"));
        // A start nobody asked for — an answer to a sign-in from before the panel was
        // reopened — is not attached to this panel: `take_start` cancels it with the daemon
        // instead, because nothing here would poll it.
        let mut other = claude_login();
        other.provider = "opencode-go".into();
        assert!(!o.login.adopt(&other));
        assert_eq!(o.login.running(), None);
        assert!(o.login.adopt(&claude_login()));
        assert_eq!(o.login.running(), Some("l1"));
        // One sign-in runs at a time, and the start that made it is spent.
        assert!(!o.login.ask("opencode-go"));
        assert!(!o.login.adopt(&claude_login()));
        // Ending it goes back to the list, ready for the next one.
        o.login.reset();
        assert_eq!(o.login.running(), None);
        assert!(o.login.ask("opencode-go"));
    }

    #[test]
    fn a_sign_in_the_gateway_stopped_answering_says_so_where_the_user_is_looking() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        o.login.providers = Some(providers());
        o.login.problem = Some("the gateway stopped answering; try /login again".into());
        let got = draw(&mut o, &s, 62, 14);
        assert!(got[2].contains("the gateway stopped answering"), "{got:?}");
        // The list is still there, so signing in again is one Enter away.
        assert!(got[4].contains("OpenCode Go"), "{got:?}");
        // Picking up another sign-in clears it: it was about the one that ended.
        assert!(o.login.ask("claude-code"));
        let got = draw(&mut o, &s, 62, 14);
        assert!(
            !got.iter().any(|l| l.contains("stopped answering")),
            "{got:?}"
        );
    }

    #[test]
    fn a_narrow_panel_keeps_every_character_of_the_url_it_shows() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Login);
        o.login.session = Some(claude_login());
        let url = "https://claude.ai/oauth/authorize?code=abcdefghijklmnop&state=0123456789";
        o.login.status = Some(LoginStatus {
            phase: "waiting".into(),
            url: Some(url.into()),
            ..Default::default()
        });
        // The URL's rows, put back together, and the widest of them: a chunk the panel had to
        // cut would lose characters with nothing to show for it.
        let gather = |got: &[String]| {
            let (mut joined, mut widest, mut inside) = (String::new(), 0, false);
            for line in got {
                let body = line.trim_start();
                let part = match body.strip_prefix("url") {
                    Some(rest) => {
                        inside = true;
                        rest.trim_start()
                    }
                    // A panel too narrow for the label opens the URL under a bare indent.
                    None if !inside && body.starts_with("http") => {
                        inside = true;
                        body
                    }
                    None if inside && !body.is_empty() && !body.contains(' ') => body,
                    None => continue,
                };
                joined.push_str(part);
                widest = widest.max(visible_width(line));
            }
            (joined, widest)
        };
        for width in [20, 25, 28, 40, 62] {
            let got = lines_at(&o, &s, width);
            let (joined, widest) = gather(&got);
            assert_eq!(joined, url, "width {width}: {got:?}");
            assert!(widest <= width - 4, "width {width}: {got:?}");
        }
        // Too narrow for a whole chunk: the head of the URL, cut, rather than a row the
        // renderer cuts mid-way with no sign of it.
        let got = lines_at(&o, &s, 14);
        let head = got
            .iter()
            .find(|l| l.contains("https"))
            .unwrap_or_else(|| panic!("no URL row: {got:?}"));
        assert!(visible_width(head) <= 10, "{got:?}");
        assert!(head.contains(glyph("…")), "{got:?}");
    }
}
