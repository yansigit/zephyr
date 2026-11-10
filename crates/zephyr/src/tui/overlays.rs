//! Panels over the bottom of the screen (the model picker, the status panel and help) and
//! slash autocomplete above the composer. Each draws from the shared [`State`]; mod.rs routes
//! the keys here while one is open.

use super::chrome::{short_model, spend, State};
use super::conn::{AccountModels, Login, LoginStatus, Provider, Snapshot, Turn, VaultEntry};
use super::settings_panel::{self, SettingsPanel};
use super::theme::{glyph, Palette};
use super::view::clean;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_tui::{bold, truncate_to_width, visible_width, CURSOR_MARKER};

/// Every slash command zephyr answers: router.rs `command` and `control`, memory.rs
/// `command` and scheduler.rs `command`, plus the TUI's own `/help` and `/p`, and the composer's
/// keys. (group, usage, what it does)
pub const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "Chat",
        "/new",
        "start a fresh conversation (or /clear); memory and rules carry over",
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
    ("Chat", "/undo <n>", "take back your last n, up to 20"),
    (
        "Chat",
        "/compact [instructions]",
        "summarise the conversation to free context",
    ),
    ("Chat", "/rename <name>", "name this conversation"),
    (
        "Chat",
        "/fork",
        "carry on in a copy; the original stays as is",
    ),
    (
        "Chat",
        "/export",
        "save it as markdown in the projects folder",
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
    ("Chat", "/p", "your saved prompts, /zephyr/prompts/*.md"),
    (
        "Chat",
        "/p <name> [args]",
        "put a saved prompt in the box, to send",
    ),
    (
        "Chat",
        "/focus",
        "hide tool calls and thinking, or show them",
    ),
    ("Chat", "/help", "this list"),
    ("Chat", "/quit", "leave (or /exit); zephyr keeps running"),
    ("Agent", "/stop", "stop the current reply and every task"),
    ("Agent", "/task list", "the agent's background tasks"),
    ("Agent", "/task stop <id>", "stop one task"),
    (
        "Agent",
        "/task parallel <n>",
        "how many tasks run at once, 1-32",
    ),
    (
        "Agent",
        "/task model <model|default>",
        "what tasks run on unless they name a model",
    ),
    (
        "Agent",
        "/subagent",
        "your subagents, which the agent starts tasks as",
    ),
    (
        "Agent",
        "/subagent add <name> [model] <prompt>",
        "add one; its prompt leads each of its tasks",
    ),
    (
        "Agent",
        "/subagent model <name> <model|default>",
        "change its model",
    ),
    ("Agent", "/subagent rm <name>", "remove one"),
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
    (
        "Agent",
        "/thinking [level|next]",
        "how hard the model thinks; Shift+Tab cycles",
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
        "Gateway",
        "/settings",
        "every setting to change, and what waits for your OK",
    ),
    (
        "Gateway",
        "/set <key> <value> [for <time>]",
        "change one setting, for good or for a while",
    ),
    (
        "Keys",
        "Up / Down",
        "your earlier messages; Up on an empty box first undoes ^C",
    ),
    ("Keys", "Ctrl+R", "search your earlier messages"),
    (
        "Keys",
        "Alt+S",
        "put the draft aside; on an empty box, bring it back",
    ),
    (
        "Keys",
        "@<path>",
        "a file in the projects folder (Tab completes); images go along",
    ),
    ("Keys", "Ctrl+G", "edit the draft in $EDITOR"),
    (
        "Keys",
        "Alt+Enter",
        "send after the current reply, not into it",
    ),
    ("Keys", "Alt+Up", "take back your messages still waiting"),
    ("Keys", "Shift+Tab", "the next thinking level"),
    (
        "Keys",
        "Ctrl+P",
        "the next model; Ctrl+Shift+P the one before",
    ),
    (
        "Keys",
        "Ctrl+F",
        "search the conversation; Enter older, Shift+Enter newer",
    ),
    ("Keys", "Ctrl+Up/Down", "jump between your messages"),
    (
        "Keys",
        "Home/End",
        "top or bottom, when the composer is empty",
    ),
    ("Keys", "Shift+click", "select from the last click and copy"),
    (
        "Keys",
        "double-click",
        "copy the word; triple-click the row",
    ),
    (
        "Keys",
        "Esc Esc",
        "rewind: pick a message to take back and edit",
    ),
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
    /// Ctrl+R: what you sent, to put back in the composer.
    History,
    /// Esc Esc: the exchanges `/undo N` counts, newest first; picking one takes it back.
    Rewind,
    /// `/settings`: every setting by section, an editor per kind, and what waits for the owner
    /// (settings spec §4.6; `settings_panel.rs`).
    Settings,
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

/// A form's fields as `Debug` prints them: a secret one by its length only.
pub(super) fn fields_debug(fields: &[(&'static str, bool, String)]) -> Vec<String> {
    (fields.iter())
        .map(|(label, secret, text)| match secret {
            true => format!("{label}: {} chars", text.chars().count()),
            false => format!("{label}: {text:?}"),
        })
        .collect()
}

/// A form's fields, one row each, the keyboard's marked: a secret field is drawn one bullet per
/// character, so onlookers learn only its length.
pub(super) fn field_rows(
    fields: &[(&'static str, bool, String)],
    at: usize,
    p: &Palette,
) -> Vec<Row> {
    let mut out = vec![];
    for (i, (label, secret, text)) in fields.iter().enumerate() {
        let shown = match secret {
            true => glyph("•").repeat(text.chars().count()),
            false => text.clone(),
        };
        let (mark, cursor) = match i == at {
            true => (p.accent.fg(glyph("›")), CURSOR_MARKER),
            false => (" ".into(), ""),
        };
        let label = p.muted.fg(&format!("{label:<10}"));
        out.push(Row::Line(format!(
            " {mark} {label}{}{cursor}",
            p.text.fg(&shown)
        )));
    }
    out
}

impl std::fmt::Debug for Form {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Form")
            .field("kind", &self.kind)
            .field("editing", &self.editing)
            .field("at", &self.at)
            .field("fields", &fields_debug(&self.fields))
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
    /// `Kind::History`: what you sent, newest first.
    pub history: Vec<String>,
    /// The `zephyr.turns` answer for the rewind picker, once it came.
    pub turns: Option<Vec<Turn>>,
    /// The settings panel's own state.
    pub settings: SettingsPanel,
    /// The first row shown and how many fit, from the last frame.
    pub(super) top: usize,
    pub(super) page: usize,
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
            history: vec![],
            turns: None,
            settings: SettingsPanel::default(),
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
    /// Change a setting: `zephyr.settings_set`, with a time limit (`2h`) or for good.
    SettingsSet {
        key: String,
        value: String,
        until: Option<String>,
    },
    /// Decide a waiting proposal: `zephyr.decide` with `ok`, `once`, `no` or `cancel`.
    Decide {
        id: i64,
        verb: String,
    },
    /// Save this private fact (`Fact`'s Debug prints only its value's length).
    FactsPut(Box<crate::facts::Fact>),
    /// Delete this private fact.
    FactsRm(String),
    /// Ask for the activity page before this cursor (`zephyr.activity` `before`).
    ActivityPage(String),
    /// Send this command as typed on the TUI: a standing grant's revoke.
    Revoke(String),
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
    /// A styled line holding the keyboard, in a panel that draws its own selection (settings):
    /// drawn as it is, and kept in view like a chosen item.
    Focus(String),
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
        Kind::History => return history_rows(o, p),
        Kind::Model => model_groups(o, snap, tz, now),
        Kind::Rewind => rewind_groups(o),
        Kind::Settings => return settings_panel::rows(o, snap, p, tz, now, width),
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
    match (o.kind, &o.turns) {
        (Kind::Rewind, None) => out.push(Row::Line(
            p.muted.fg(&format!("listing your messages{}", glyph("…"))),
        )),
        (Kind::Rewind, Some(t)) if t.is_empty() => {
            out.push(Row::Line(p.muted.fg("nothing to undo")))
        }
        _ if out.is_empty() => out.push(Row::Line(p.muted.fg("nothing matches"))),
        _ => {}
    }
    if o.kind == Kind::Model && o.models.is_none() {
        out.push(Row::Line(
            p.muted.fg(&format!("listing models{}", glyph("…"))),
        ));
    }
    out
}

/// Each exchange, newest first: picking the k-th sends `/undo k`.
fn rewind_groups(o: &Overlay) -> Vec<Group> {
    let items = (o.turns.iter().flatten().enumerate())
        .map(|(i, t)| Row::Item {
            pick: format!("/undo {}", i + 1),
            label: t.text.split_whitespace().collect::<Vec<_>>().join(" "),
            note: match t.channel.as_str() {
                crate::attach::CHANNEL => "you".into(),
                channel => channel.into(),
            },
            current: false,
        })
        .collect();
    let note = "takes it and everything after back; files stay changed (see /rollback)";
    vec![("rewind to before".into(), note.into(), items)]
}

/// The message a rewind pick (`/undo k`) takes back, for the composer.
pub fn rewound(o: &Overlay, pick: &str) -> Option<String> {
    let k: usize = pick.strip_prefix("/undo ")?.parse().ok()?;
    Some(o.turns.as_ref()?.get(k.checked_sub(1)?)?.text.clone())
}

pub(super) type Group = (String, String, Vec<Row>);

/// Ctrl+R's rows: every entry holding what was typed, one line each.
fn history_rows(o: &Overlay, p: &Palette) -> Vec<Row> {
    let query = o.query.to_lowercase();
    let rows: Vec<Row> = (o.history.iter())
        .filter(|e| e.to_lowercase().contains(&query))
        .map(|e| Row::Item {
            pick: e.clone(),
            label: e
                .lines()
                .collect::<Vec<_>>()
                .join(&format!(" {} ", glyph("⏎"))),
            note: String::new(),
            current: false,
        })
        .collect();
    match (rows.is_empty(), o.history.is_empty()) {
        (false, _) => rows,
        (true, true) => vec![Row::Line(p.muted.fg("nothing sent yet"))],
        (true, false) => vec![Row::Line(p.muted.fg("nothing matches"))],
    }
}

fn help_groups() -> Vec<Group> {
    let mut groups: Vec<Group> = vec![];
    for (group, usage, what) in COMMANDS {
        if groups.last().is_none_or(|g| g.0 != *group) {
            groups.push((group.to_string(), String::new(), vec![]));
        }
        let item = Row::Item {
            // A key is pressed, not typed into the composer.
            pick: match *group {
                "Keys" => String::new(),
                _ => insert(usage).to_string(),
            },
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
pub(super) fn model_groups(
    o: &Overlay,
    snap: &Snapshot,
    tz: &jiff::tz::TimeZone,
    now: i64,
) -> Vec<Group> {
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
    out.push(Row::Line(
        p.text.fg(phase_text(&status, session.kind == "oauth")),
    ));
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
fn phase_text(status: &LoginStatus, oauth: bool) -> &'static str {
    match status.phase.as_str() {
        // An OAuth sign-in under the network floor: the browser first.
        "need_input" if oauth => {
            "waiting for you to finish in the browser; if it then cannot open 127.0.0.1, paste its address here"
        }
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
        out.extend(field_rows(&form.fields, form.at, p));
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

/// A paste into the vault form, a settings editor or the facts form goes where typing would.
pub fn paste(o: &mut Overlay, text: &str) {
    if let Some(form) = o.vault.form.as_mut().filter(|_| o.kind == Kind::Vault) {
        form.type_text(text);
    }
    if o.kind == Kind::Settings {
        settings_panel::paste(o, text);
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
        // The settings panel's model editor draws its own filter line, among its rows.
        Kind::Status | Kind::Vault | Kind::Settings => false,
        Kind::Login => o.login.running().is_none(),
        Kind::Model | Kind::Help | Kind::History | Kind::Rewind => true,
    }
}

/// The positions of the pickable rows.
pub(super) fn picks(rows: &[Row]) -> Vec<usize> {
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
    if o.kind == Kind::Settings {
        return settings_panel::key(o, k, rows);
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
        Kind::History => " history ",
        Kind::Login => " sign in ",
        Kind::Vault => " vault ",
        Kind::Rewind => " rewind ",
        Kind::Settings => " settings ",
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
    // The chosen item, else the row a panel marked as holding the keyboard.
    let focus = (picks.get(o.selected).copied())
        .or_else(|| rows.iter().position(|r| matches!(r, Row::Focus(_))));
    if let Some(at) = focus {
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
                // The model picker and the settings editors mark the value in use.
                let dot = match (o.kind, current) {
                    (Kind::Model | Kind::Settings, true) => format!("{} ", p.accent.fg(glyph("●"))),
                    (Kind::Model | Kind::Settings, false) => "  ".into(),
                    _ => String::new(),
                };
                let label = format!(
                    "{label}{}",
                    " ".repeat(label_w.saturating_sub(visible_width(label)))
                );
                let label = if sel { bold(&label) } else { label };
                format!(" {mark} {dot}{}  {}", p.text.fg(&label), p.muted.fg(note))
            }
            Row::Line(text) | Row::Focus(text) => text.clone(),
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
pub(super) fn column<'a>(labels: impl Iterator<Item = &'a str>) -> usize {
    const MAX: usize = 26;
    labels
        .map(visible_width)
        .filter(|&w| w <= MAX)
        .max()
        .unwrap_or(0)
}

/// Autocomplete above the composer: what the text up to the cursor could still become.
#[derive(Debug, Clone, PartialEq)]
pub struct Complete {
    pub items: Vec<Choice>,
    pub selected: usize,
}

/// One autocomplete row, from any source: a command, an argument, a saved prompt or a file.
#[derive(Debug, Clone, PartialEq)]
pub struct Choice {
    pub label: String,
    pub detail: String,
    /// The text up to the cursor once it is taken; what follows the cursor stays. Equal to
    /// the text already typed when there is nothing to complete (a command's arguments).
    pub insert: String,
    /// Enter takes it and sends: a command that needs nothing more. Otherwise Enter only
    /// takes it.
    pub then_run: bool,
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

/// The slash commands as choices: one still being named completes to its name, one whose
/// arguments are being typed shows its usage and leaves the text alone.
pub fn slash(text: &str) -> Vec<Choice> {
    (matches(text).into_iter())
        .map(|i| {
            let (_, usage, what) = COMMANDS[i];
            let ins = insert(usage);
            Choice {
                label: usage.to_string(),
                detail: what.to_string(),
                insert: if ins.starts_with(text) { ins } else { text }.to_string(),
                then_run: !ins.ends_with(' '),
            }
        })
        .collect()
}

/// A command's argument, from what the TUI already holds; `codes` is how many code blocks the
/// transcript has.
pub fn args(text: &str, s: &State, codes: usize) -> Vec<Choice> {
    let Some((head, partial)) = text.rsplit_once(' ').filter(|_| !text.contains('\n')) else {
        return vec![];
    };
    let mut options: Vec<(String, String)> = match head {
        "/model" => {
            let named = |a: &AccountModels| a.name.clone().unwrap_or_default();
            let mut models: Vec<(String, String)> = (s.models.iter())
                .flat_map(|a| a.models.iter().map(move |m| (m.clone(), named(a))))
                .collect();
            // Until `zephyr.models` has answered, each account's own model.
            if models.is_empty() {
                models = (s.snapshot.accounts.iter())
                    .map(|a| (a.model.clone(), a.name.clone()))
                    .chain(s.snapshot.model.clone().map(|m| (m, String::new())))
                    .collect();
            }
            models
        }
        "/copy" => (1..=codes)
            .map(|n| (n.to_string(), String::new()))
            .collect(),
        "/job run" | "/job rm" => (s.snapshot.jobs.iter())
            .map(|j| (j.id.to_string(), j.name.clone()))
            .collect(),
        "/vault rm" | "/vault edit" | "/vault revoke" => {
            s.vault.iter().map(|n| (n.clone(), String::new())).collect()
        }
        "/switch" => (s.sessions.iter())
            .map(|x| (x.id.clone(), x.name.clone().unwrap_or_default()))
            .collect(),
        _ => return vec![],
    };
    let mut seen = std::collections::HashSet::new();
    options.retain(|(value, _)| seen.insert(value.clone()));
    let partial = partial.to_lowercase();
    (options.into_iter())
        // A `/rename` name or a job's finds its id too.
        .filter(|(value, detail)| {
            value.to_lowercase().contains(&partial) || detail.to_lowercase().contains(&partial)
        })
        .map(|(value, detail)| Choice {
            insert: format!("{head} {value}"),
            label: value,
            detail,
            then_run: true,
        })
        .collect()
}

/// `/p <name>`: the saved prompts, `(name, first line)`, whose names start with what is typed.
pub fn prompt_args(text: &str, prompts: &[(String, String)]) -> Vec<Choice> {
    let Some(partial) = (text.strip_prefix("/p ")).filter(|p| !p.contains([' ', '\n'])) else {
        return vec![];
    };
    (prompts.iter())
        .filter(|(name, _)| name.starts_with(partial))
        .map(|(name, first)| Choice {
            label: name.clone(),
            detail: first.clone(),
            insert: format!("/p {name}"),
            then_run: true,
        })
        .collect()
}

/// The rows above the composer: at most six, scrolled to the selection.
pub fn completions(s: &State, p: &Palette, width: usize, _now: i64) -> Vec<String> {
    const ROWS: usize = 6;
    let Some(c) = &s.complete else { return vec![] };
    let top = (c.selected + 1).saturating_sub(ROWS);
    let shown: Vec<(usize, &Choice)> = c.items.iter().enumerate().skip(top).take(ROWS).collect();
    let w = column(shown.iter().map(|(_, c)| c.label.as_str()));
    shown
        .into_iter()
        .map(|(k, choice)| {
            let sel = k == c.selected;
            let mark = if sel {
                p.accent.fg(glyph("›"))
            } else {
                " ".into()
            };
            let label = format!(
                "{}{}",
                choice.label,
                " ".repeat(w.saturating_sub(visible_width(&choice.label)))
            );
            let label = if sel { bold(&label) } else { label };
            let row = format!(
                " {mark} {}  {}",
                p.text.fg(&label),
                p.muted.fg(&choice.detail)
            );
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

    /// From the settings panel's left column to Waiting, above Activity and Grants.
    fn to_waiting(o: &mut Overlay, s: &Snapshot) {
        for code in [KeyCode::End, KeyCode::Up, KeyCode::Up] {
            hit(o, s, code);
        }
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
        assert!(got[7].ends_with(" 1-5 of 91 ─╯"), "{got:?}");
        for _ in 0..40 {
            key(&mut o, press(KeyCode::Down), &rows);
        }
        let got = plain(&render(&mut o, &rows, &DARK, 80, 8));
        // The forty-first command is /memory ok; the window follows it down.
        assert_eq!(
            got[6],
            "│  › /memory ok                  let the agent see the waiting change          │"
        );
        assert!(got[7].ends_with(" 40-44 of 91 ─╯"), "{got:?}");
        key(&mut o, press(KeyCode::Down), &rows);
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick("/memory undo".into())
        );
        // The last row is a key, which puts nothing in the composer.
        key(&mut o, press(KeyCode::End), &rows);
        let got = plain(&render(&mut o, &rows, &DARK, 80, 8));
        assert_eq!(
            got[6],
            "│  › Esc Esc                     rewind: pick a message to take back and edit  │"
        );
        // A key is not something to put in the composer.
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick(String::new())
        );
        for _ in 0..5 {
            key(&mut o, press(KeyCode::Up), &rows);
        }
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick(String::new())
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
    fn the_rewind_picker_lists_turns_newest_first_and_picks_undo_k() {
        let s = Snapshot::default();
        let mut o = Overlay::new(Kind::Rewind);
        assert!(draw(&mut o, &s, 80, 12)[2].contains("listing your messages"));
        o.turns = Some(vec![
            Turn {
                channel: "tui".into(),
                text: "second\nline".into(),
            },
            Turn {
                channel: "discord".into(),
                text: "first".into(),
            },
        ]);
        let got = draw(&mut o, &s, 60, 12);
        assert_eq!(
            got[0],
            "╭─ rewind ─────────────────────────────────────────────────╮"
        );
        assert!(got[2].starts_with("│ rewind to before  takes it and everything after back;"));
        assert_eq!(
            got[3],
            "│  › second line  you                                      │"
        );
        assert_eq!(
            got[4],
            "│    first        discord                                  │"
        );
        assert_eq!(hit(&mut o, &s, KeyCode::Down), Action::None);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::Pick("/undo 2".into())
        );
        assert_eq!(rewound(&o, "/undo 2").as_deref(), Some("first"));
        assert_eq!(rewound(&o, "/undo 3"), None);
        o.turns = Some(vec![]);
        assert!(draw(&mut o, &s, 60, 12)[2].contains("nothing to undo"));
    }

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
                        // A rule is a setting: the router's `set_setting` calls `memory::rule`.
                        || (text.strip_prefix("/rule "))
                            .is_some_and(|arg| crate::memory::rule(&dir, arg).is_ok())
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

    /// `zephyr.settings`' answer, cut down: a model, a choice, an optional number with a time
    /// limit running, a bool the agent changed, a bounded number and a read-only entry.
    fn settings_answer() -> serde_json::Value {
        use serde_json::json;
        let row = |key: &str, label: &str, kind: serde_json::Value, value: Option<&str>| {
            json!({"key": key, "label": label, "help": format!("help for {key}"), "kind": kind,
                   "value": value, "source": "default", "access": "propose", "strong": false,
                   "until": null})
        };
        let with = |mut v: serde_json::Value, field: &str, x: serde_json::Value| {
            v[field] = x;
            v
        };
        let float =
            json!({"type": "optional", "inner": {"type": "float", "min": 0.0, "max": 1.0e9}});
        let cost = row(
            "budget.daily_cost_usd",
            "Daily cost (USD)",
            float,
            Some("5"),
        );
        let cost = with(
            with(cost, "source", json!("until 15:30")),
            "strong",
            json!(true),
        );
        let cost = with(cost, "access", json!("tighten"));
        let turns = json!({"type": "optional", "inner": {"type": "int", "min": 1, "max": 100000}});
        let refresh = row(
            "auth.enabled",
            "Token refresh",
            json!({"type": "bool"}),
            Some("true"),
        );
        json!({"sections": [
            {"name": "Model", "settings": [
                with(row("model", "Model", json!({"type": "model"}), Some("anthropic/claude-opus-5")),
                     "source", json!("you")),
                row("thinking", "Thinking level",
                    json!({"type": "choice", "options": ["off", "low", "medium", "high"]}), Some("medium")),
            ]},
            {"name": "Budget", "settings": [cost, row("budget.run_turns", "Turns per run", turns, None)]},
            {"name": "Accounts", "settings": [
                with(refresh, "source", json!("agent (#4)")),
                row("auth.refresh_minutes", "Refresh check (minutes)",
                    json!({"type": "int", "min": 1, "max": 60}), Some("5")),
            ]},
            {"name": "Read-only", "settings": [
                with(row("owners", "Owners", json!({"type": "text", "max": 0}), None),
                     "access", json!("read-only")),
            ]},
        ], "strong_channels": ["tui", "chat"]})
    }

    fn proposals_answer() -> serde_json::Value {
        serde_json::json!({"proposals": [
            {"id": 12, "card": "/set budget.daily_cost_usd 8 for 2h", "kind": "setting",
             "key": "budget.daily_cost_usd", "age_secs": 180, "expires_in_secs": 86_220, "strong": true},
            {"id": 11, "card": "/set tasks.parallel 4", "kind": "setting", "key": "tasks.parallel",
             "age_secs": 7_200, "expires_in_secs": 79_200, "strong": false},
        ]})
    }

    fn settings_panel() -> Overlay {
        let mut o = Overlay::new(Kind::Settings);
        o.settings.load(&settings_answer());
        o
    }

    fn typing(o: &mut Overlay, s: &Snapshot, text: &str) {
        for c in text.chars() {
            assert_eq!(hit(o, s, KeyCode::Char(c)), Action::None, "{c:?}");
        }
    }

    #[test]
    fn settings_panel_lists_sections_and_sources() {
        let s = snap();
        let mut o = Overlay::new(Kind::Settings);
        let early = draw(&mut o, &s, 80, 20);
        assert_eq!(
            early[0],
            "╭─ settings ───────────────────────────────────────────────────────────────────╮"
        );
        assert!(early[1].contains("listing settings…"), "{early:?}");
        o.settings.load(&settings_answer());
        o.settings.load_waiting(&proposals_answer());
        let got = draw(&mut o, &s, 80, 20);
        // Left: the sections, then Waiting with its count; right: the open section's rows,
        // each `label · value · source`.
        assert_eq!(
            got[1],
            "│ › Model           Model           anthropic/claude-opus-5 · you              │"
        );
        assert_eq!(
            got[2],
            "│   Budget          Thinking level  medium                  · default          │"
        );
        assert!(got[3].starts_with("│   Accounts  "), "{got:?}");
        assert!(got[4].starts_with("│   Read-only  "), "{got:?}");
        assert!(got[5].starts_with("│   Waiting (2)  "), "{got:?}");
        // Down opens the next section; its sources say who set each value, and until when.
        hit(&mut o, &s, KeyCode::Down);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got[1].contains("  Daily cost (USD)  5    · until 15:30"),
            "{got:?}"
        );
        assert!(
            got[2].contains("› Budget          Turns per run     none · default"),
            "{got:?}"
        );
        // Right moves the keyboard onto the rows: the chosen one says what it is and takes.
        hit(&mut o, &s, KeyCode::Right);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[1].contains("› Daily cost (USD)  5"), "{got:?}");
        // The row's key leads what it is: two rows with one label are still told apart.
        assert!(
            got.iter()
                .any(|l| l.contains("budget.daily_cost_usd · help for budget.daily_cost_usd")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|l| l.contains("a number from 0, or none · the agent may tighten it  ")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|l| l.contains("loosening it needs tui or chat")),
            "{got:?}"
        );
        hit(&mut o, &s, KeyCode::Down);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[2].contains("› Turns per run     none"), "{got:?}");
        // The agent's change names its proposal; read-only entries are drawn muted.
        hit(&mut o, &s, KeyCode::Left);
        hit(&mut o, &s, KeyCode::Down);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got[1].contains("Token refresh            true · agent (#4)"),
            "{got:?}"
        );
        hit(&mut o, &s, KeyCode::Down);
        let lines: Vec<String> = (list(&o, &s).into_iter())
            .filter_map(|r| match r {
                Row::Line(t) | Row::Focus(t) => Some(t),
                _ => None,
            })
            .collect();
        let owners = lines.iter().find(|l| l.contains("Owners")).unwrap();
        assert!(
            owners.contains(&DARK.muted.fg("Owners  help for owners")),
            "{owners:?}"
        );
        // Enter on a read-only row says where it changes instead of opening an editor.
        hit(&mut o, &s, KeyCode::Right);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        assert!(o.settings.editing.is_none());
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("Owners is read-only here: edit zephyr.toml")),
            "{got:?}"
        );
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::Close);
    }

    #[test]
    fn a_bool_toggles_with_space_and_sends_settings_set() {
        let s = snap();
        let mut o = settings_panel();
        draw(&mut o, &s, 80, 20);
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Right);
        // Space toggles at once: the change goes out, with no editor in between.
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char(' ')),
            Action::SettingsSet {
                key: "auth.enabled".into(),
                value: "false".into(),
                until: None,
            }
        );
        assert!(o.settings.editing.is_none());
        // `t` makes the same change for a while: the other value is chosen, the keyboard on
        // the time limit.
        assert_eq!(hit(&mut o, &s, KeyCode::Char('t')), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[1].contains("Token refresh  auth.enabled"), "{got:?}");
        assert!(got.iter().any(|l| l.contains("  ● true")), "{got:?}");
        assert!(got.iter().any(|l| l.contains("›   false")), "{got:?}");
        assert!(got.iter().any(|l| l.contains("› for")), "{got:?}");
        typing(&mut o, &s, "2h");
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::SettingsSet {
                key: "auth.enabled".into(),
                value: "false".into(),
                until: Some("2h".into()),
            }
        );
        // The change closes the editor and is said where the owner is looking.
        o.settings.answered(
            &serde_json::json!({"outcome": "changed", "key": "auth.enabled", "old": "true",
                "new": "false", "applied": "[zephyr] applied", "until": NOW / 1000 + 7200}),
            &jiff::tz::TimeZone::UTC,
        );
        assert!(o.settings.editing.is_none());
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("auth.enabled true → false until 16:02")),
            "{got:?}"
        );
        // Space leaves a number alone.
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(hit(&mut o, &s, KeyCode::Char(' ')), Action::None);
        assert!(o.settings.editing.is_none());
    }

    #[test]
    fn a_refused_edit_shows_its_reason() {
        let s = snap();
        let mut o = settings_panel();
        draw(&mut o, &s, 80, 20);
        hit(&mut o, &s, KeyCode::Down);
        // Enter opens the section, then edits the row in a field that fits its kind.
        hit(&mut o, &s, KeyCode::Enter);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got[1].contains("Daily cost (USD)  budget.daily_cost_usd"),
            "{got:?}"
        );
        assert!(
            got.iter().any(|l| l.contains("a number from 0, or none")),
            "{got:?}"
        );
        assert!(got.iter().any(|l| l.contains("› value  5")), "{got:?}");
        hit(&mut o, &s, KeyCode::Backspace);
        typing(&mut o, &s, "-1");
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::SettingsSet {
                key: "budget.daily_cost_usd".into(),
                value: "-1".into(),
                until: None,
            }
        );
        let reason = "not changed: [budget] daily_cost_usd must be 0 to 1000000000";
        o.settings.answered(
            &serde_json::json!({"outcome": "refused", "reason": reason}),
            &jiff::tz::TimeZone::UTC,
        );
        // The editor stays, with what was typed and why it was refused, in the error colour.
        let got = draw(&mut o, &s, 80, 20);
        assert!(got.iter().any(|l| l.contains("› value  -1")), "{got:?}");
        assert!(got.iter().any(|l| l.contains(reason)), "{got:?}");
        let rows = list(&o, &s);
        assert!(rows.contains(&Row::Line(DARK.error.fg(reason))), "{rows:?}");
        // Fixed and sent again it goes out; Esc leaves the editor, and the reason with it.
        hit(&mut o, &s, KeyCode::Backspace);
        hit(&mut o, &s, KeyCode::Backspace);
        typing(&mut o, &s, "8");
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::SettingsSet {
                key: "budget.daily_cost_usd".into(),
                value: "8".into(),
                until: None,
            }
        );
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(!got.iter().any(|l| l.contains("must be")), "{got:?}");
        assert!(got[1].contains("› Daily cost (USD)"), "{got:?}");
        // The answer to the edit left behind says what changed, and leaves the editor opened since.
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Enter);
        o.settings.answered(
            &serde_json::json!({"outcome": "changed", "key": "budget.daily_cost_usd", "old": "5",
                "new": "8", "applied": "[zephyr] applied", "until": null}),
            &jiff::tz::TimeZone::UTC,
        );
        let editing = o.settings.editing.as_ref().map(|e| e.key.as_str());
        assert_eq!(editing, Some("budget.run_turns"));
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("budget.daily_cost_usd 5 → 8")),
            "{got:?}"
        );
    }

    #[test]
    fn a_model_setting_edits_in_the_model_picker_and_a_choice_in_a_list() {
        let s = snap();
        let mut o = settings_panel();
        o.models = Some(models());
        draw(&mut o, &s, 80, 20);
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Enter);
        // The model picker's groups, the model in effect marked; typing filters them.
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter().any(|l| l.contains("› type to filter")),
            "{got:?}"
        );
        assert!(got.iter().any(|l| l.contains("work  in use")), "{got:?}");
        assert!(
            got.iter().any(|l| l.contains("› ● claude-opus-5")),
            "{got:?}"
        );
        assert!(got.iter().any(|l| l.contains("    gpt-6")), "{got:?}");
        typing(&mut o, &s, "sonn");
        let got = draw(&mut o, &s, 80, 20);
        assert!(!got.iter().any(|l| l.contains("gpt-6")), "{got:?}");
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::SettingsSet {
                key: "model".into(),
                value: "anthropic/claude-sonnet-5".into(),
                until: None,
            }
        );
        hit(&mut o, &s, KeyCode::Esc);
        // A choice is a list starting on the value in effect.
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Enter);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got.iter().any(|l| l.contains("› ● medium")), "{got:?}");
        assert!(
            got.iter()
                .any(|l| l.contains("one of off, low, medium, high")),
            "{got:?}"
        );
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::SettingsSet {
                key: "thinking".into(),
                value: "high".into(),
                until: None,
            }
        );
    }

    #[test]
    fn the_waiting_cursor_stays_on_the_proposal_the_owner_picked() {
        let s = snap();
        let mut o = settings_panel();
        o.settings.load_waiting(&proposals_answer());
        draw(&mut o, &s, 80, 20);
        to_waiting(&mut o, &s);
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Down);
        assert!(draw(&mut o, &s, 80, 20)[2].contains("› #11"));
        // The agent files #13 meanwhile. Newest first, #11 moves down a row; the cursor with it.
        let thirteen = serde_json::json!({"id": 13, "card": "/set tasks.parallel 1",
            "kind": "setting", "key": "tasks.parallel", "age_secs": 5,
            "expires_in_secs": 86_395, "strong": false});
        let mut more = proposals_answer();
        more["proposals"]
            .as_array_mut()
            .unwrap()
            .insert(0, thirteen);
        o.settings.load_waiting(&more);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('o')),
            Action::Decide {
                id: 11,
                verb: "ok".into()
            }
        );
        assert!(draw(&mut o, &s, 80, 20)[3].contains("› #11"));
        // #11 is decided elsewhere: the cursor goes to the nearest row, and the next o, 1 or d
        // only says so, since the owner never saw that row chosen.
        let mut gone = more.clone();
        gone["proposals"].as_array_mut().unwrap().remove(2);
        o.settings.load_waiting(&gone);
        assert_eq!(hit(&mut o, &s, KeyCode::Char('o')), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[2].contains("› #12"), "{got:?}");
        assert!(
            got.iter()
                .any(|l| l.contains("#11 is no longer waiting; the cursor is on #12")),
            "{got:?}"
        );
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('o')),
            Action::Decide {
                id: 12,
                verb: "ok".into()
            }
        );
        // Moving on also counts as having seen the row.
        gone["proposals"].as_array_mut().unwrap().remove(1);
        o.settings.load_waiting(&gone);
        hit(&mut o, &s, KeyCode::Up);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('d')),
            Action::Decide {
                id: 13,
                verb: "no".into()
            }
        );
    }

    #[test]
    fn what_the_daemon_sends_is_cleaned_before_it_is_drawn() {
        let s = snap();
        let mut answer = settings_answer();
        let model = &mut answer["sections"][0]["settings"][0];
        model["value"] = serde_json::json!("Asia/\x1b[31mSeoul\u{202e}x");
        model["source"] = serde_json::json!("you\x1b]0;pwned\x07\u{2066}");
        model["label"] = serde_json::json!("Mo\u{7}del\nX");
        let mut o = Overlay::new(Kind::Settings);
        o.settings.load(&answer);
        o.settings.load_waiting(&serde_json::json!({"proposals": [
            {"id": 12, "card": "/set timezone Asia/\x1b[2JSeoul\u{202e}", "kind": "setting",
             "key": "time\u{2066}zone", "age_secs": 1, "expires_in_secs": 9, "strong": false}]}));
        let bad = [
            "\x1b[31m", "\x1b]0;", "\x1b[2J", "\x07", "\u{202e}", "\u{2066}", "\n",
        ];
        let check = |o: &Overlay| {
            for row in list(o, &s) {
                let text = match row {
                    Row::Line(t) | Row::Focus(t) => t,
                    Row::Head(a, b) => format!("{a}{b}"),
                    Row::Item { label, note, .. } => format!("{label}{note}"),
                };
                for b in bad {
                    assert!(!text.contains(b), "{b:?} reached {text:?}");
                }
            }
        };
        check(&o);
        let got = draw(&mut o, &s, 90, 20);
        assert!(
            got[1].contains("Model X") && got[1].contains("Asia/Seoulx · you"),
            "{got:?}"
        );
        // An editor, the Waiting list and the card are drawn from the same cleaned text.
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Enter);
        assert!(o.settings.editing.is_some());
        check(&o);
        hit(&mut o, &s, KeyCode::Esc);
        hit(&mut o, &s, KeyCode::Left);
        to_waiting(&mut o, &s);
        hit(&mut o, &s, KeyCode::Right);
        check(&o);
        hit(&mut o, &s, KeyCode::Enter);
        assert!(o.settings.card.is_some());
        check(&o);
        let card = draw(&mut o, &s, 90, 20);
        assert!(
            card.iter().any(|l| l.contains("/set timezone Asia/Seoul")),
            "{card:?}"
        );
    }

    #[test]
    fn waiting_tab_approves_with_o_and_drops_with_d() {
        let s = snap();
        let mut o = settings_panel();
        o.settings.load_waiting(&proposals_answer());
        draw(&mut o, &s, 80, 20);
        to_waiting(&mut o, &s);
        // Newest first, each with how long it has waited.
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got[1].contains("#12  /set budget.daily_cost_usd 8 for 2h"),
            "{got:?}"
        );
        assert!(got[1].trim_end_matches(" │").ends_with("3m ago"), "{got:?}");
        assert!(got[2].contains("#11  /set tasks.parallel 4"), "{got:?}");
        assert!(got[2].trim_end_matches(" │").ends_with("2h ago"), "{got:?}");
        assert!(got[5].starts_with("│ › Waiting (2)"), "{got:?}");
        hit(&mut o, &s, KeyCode::Right);
        // Enter shows the whole card; Esc goes back to the list.
        hit(&mut o, &s, KeyCode::Enter);
        let card = draw(&mut o, &s, 80, 20);
        assert!(card[1].contains("proposal #12"), "{card:?}");
        assert!(
            card.iter()
                .any(|l| l.contains("/set budget.daily_cost_usd 8 for 2h")),
            "{card:?}"
        );
        assert!(
            card.iter()
                .any(|l| l.contains("waited 3m · expires in 23h 57m")),
            "{card:?}"
        );
        // What it would change, as it stands.
        assert!(
            card.iter().any(|l| l.contains("now 5 · until 15:30")),
            "{card:?}"
        );
        assert!(
            card.iter()
                .any(|l| l.contains("needs a strong channel: tui or chat")),
            "{card:?}"
        );
        assert_eq!(hit(&mut o, &s, KeyCode::Esc), Action::None);
        assert!(!draw(&mut o, &s, 80, 20)[1].contains("proposal #12"));
        hit(&mut o, &s, KeyCode::Enter);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('o')),
            Action::Decide {
                id: 12,
                verb: "ok".into()
            }
        );
        o.settings.decided(&serde_json::json!({"ok": true,
            "text": "[zephyr] /ok 12 approved /set budget.daily_cost_usd 8 for 2h\n[zephyr] budget.daily_cost_usd 5 → 8 until 16:02"}));
        let mut left = proposals_answer();
        left["proposals"].as_array_mut().unwrap().remove(0);
        o.settings.load_waiting(&left);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[1].contains("› #11  /set tasks.parallel 4"), "{got:?}");
        assert!(got[5].starts_with("│ › Waiting (1)"), "{got:?}");
        assert!(
            got.iter()
                .any(|l| l.contains("budget.daily_cost_usd 5 → 8 until 16:02")),
            "{got:?}"
        );
        // The cursor moved to #11 with the reload, unseen: the first `1` only says so. Then `1`
        // approves once (the same as ok for a setting); `d` drops.
        assert_eq!(hit(&mut o, &s, KeyCode::Char('1')), Action::None);
        assert!(draw(&mut o, &s, 80, 20)
            .iter()
            .any(|l| l.contains("#12 is no longer waiting; the cursor is on #11")),);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('1')),
            Action::Decide {
                id: 11,
                verb: "once".into()
            }
        );
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('d')),
            Action::Decide {
                id: 11,
                verb: "no".into()
            }
        );
        o.settings
            .load_waiting(&serde_json::json!({"proposals": []}));
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[1].contains("nothing waits for you"), "{got:?}");
        assert!(got[5].starts_with("│ › Waiting (0)"), "{got:?}");
    }

    /// `zephyr.settings` with its Approvals section, cut down (made-up values). `modified`: a
    /// hand-set value differs from the preset.
    fn approvals_answer(modified: bool) -> serde_json::Value {
        use serde_json::json;
        let row = |key: &str, label: &str, kind: serde_json::Value, value: &str, source: &str| {
            json!({"key": key, "label": label, "help": format!("help for {key}"), "kind": kind,
                   "value": value, "source": source, "access": "tighten", "strong": true,
                   "until": null, "modified": false})
        };
        let mode = |options: &[&str]| json!({"type": "optional", "inner": {"type": "mode", "options": options}});
        let mut preset = row(
            "approvals.preset",
            "Preset",
            json!({"type": "choice", "options": ["manual", "semi", "full"]}),
            "semi",
            "you",
        );
        preset["modified"] = json!(modified);
        let classes = ["ask", "ask-new", "delay N", "auto"];
        json!({"sections": [
            {"name": "Model", "settings": [
                row("model", "Model", json!({"type": "model"}), "anthropic/claude-opus-5", "you")]},
            {"name": "Approvals", "settings": [
                row("facts", "Private facts", json!({"type": "text", "max": 4000}), "home-address", "you"),
                preset,
                row("approvals.messages", "Messages", mode(&classes), "ask", "you"),
                row("approvals.posts", "Posts", mode(&classes), "delay 5", "preset"),
                row("approvals.money", "Payments", mode(&["ask", "delay N", "auto", "auto N"]),
                    "auto 50", "you"),
                row("approvals.currency", "Currency", json!({"type": "text", "max": 3}), "EUR", "you"),
                row("approvals.facts", "Facts check",
                    json!({"type": "optional", "inner": {"type": "choice", "options": ["hold", "warn", "off"]}}),
                    "hold", "preset"),
            ]},
            {"name": "Read-only", "settings": [
                {"key": "owners", "label": "Owners", "kind": {"type": "text", "max": 0},
                 "access": "read-only"}]},
        ], "strong_channels": ["tui", "chat"]})
    }

    /// The line of `got` that holds `label`.
    fn line_of<'a>(got: &'a [String], label: &str) -> &'a str {
        let found = got.iter().find(|l| l.contains(label));
        found.unwrap_or_else(|| panic!("no {label:?} in {got:?}"))
    }

    #[test]
    fn approvals_rows_show_the_preset_and_modified() {
        let s = snap();
        let mut o = Overlay::new(Kind::Settings);
        o.settings.load(&approvals_answer(true));
        o.settings
            .load_waiting(&serde_json::json!({"proposals": []}));
        let got = draw(&mut o, &s, 80, 20);
        // Private facts sit right after Approvals; Waiting stays last.
        assert!(got[2].starts_with("│   Approvals  "), "{got:?}");
        assert!(got[3].starts_with("│   Private facts  "), "{got:?}");
        assert!(got[4].starts_with("│   Read-only  "), "{got:?}");
        assert!(got[5].starts_with("│   Waiting (0)  "), "{got:?}");
        // Waiting still comes after the facts.
        to_waiting(&mut o, &s);
        assert!(draw(&mut o, &s, 80, 20)[5].starts_with("│ › Waiting (0)"));
        hit(&mut o, &s, KeyCode::Home);
        hit(&mut o, &s, KeyCode::Down);
        let got = draw(&mut o, &s, 80, 20);
        // The preset says when a hand-set value differs from it; each class shows its mode and
        // where it came from, the money cap and its currency too.
        let preset = line_of(&got, "Preset");
        assert!(preset.contains("semi (modified) · you"), "{got:?}");
        assert!(line_of(&got, "Messages").contains("ask "), "{got:?}");
        assert!(line_of(&got, "Posts").contains("delay 5"), "{got:?}");
        assert!(line_of(&got, "Posts").contains("· preset"), "{got:?}");
        assert!(line_of(&got, "Payments").contains("auto 50"), "{got:?}");
        assert!(line_of(&got, "Currency").contains("EUR"), "{got:?}");
        o.settings.load(&approvals_answer(false));
        let got = draw(&mut o, &s, 80, 20);
        assert!(!line_of(&got, "Preset").contains("modified"), "{got:?}");
        assert!(line_of(&got, "Preset").contains("semi "), "{got:?}");
        // The preset's list marks the preset itself, without the note.
        o.settings.load(&approvals_answer(true));
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got.iter().any(|l| l.contains("● semi")), "{got:?}");
        hit(&mut o, &s, KeyCode::Esc);
        // A class's mode is typed; the registry refuses what it does not take, inline.
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Enter);
        for _ in 0..3 {
            hit(&mut o, &s, KeyCode::Backspace);
        }
        typing(&mut o, &s, "sometimes");
        assert_eq!(
            hit(&mut o, &s, KeyCode::Enter),
            Action::SettingsSet {
                key: "approvals.messages".into(),
                value: "sometimes".into(),
                until: None,
            }
        );
        let why = "[zephyr] not changed: messages is ask, ask-new, delay N or auto";
        o.settings.answered(
            &serde_json::json!({"outcome": "refused", "reason": why}),
            &jiff::tz::TimeZone::UTC,
        );
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("not changed: messages is ask, ask-new")),
            "{got:?}"
        );
        assert!(
            got.iter().any(|l| l.contains("› value  sometimes")),
            "{got:?}"
        );
        hit(&mut o, &s, KeyCode::Esc);
        // Clearing a class goes back to the preset: `default`, never a `none` beside `off`.
        hit(&mut o, &s, KeyCode::End);
        hit(&mut o, &s, KeyCode::Enter);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("one of hold, warn, off, or default (the preset's)")),
            "{got:?}"
        );
        assert!(got.iter().any(|l| l.contains("  default")), "{got:?}");
        assert!(!got.iter().any(|l| l.contains(" none")), "{got:?}");
        hit(&mut o, &s, KeyCode::Esc);
        // The facts' row leads to their own section, where values are typed.
        hit(&mut o, &s, KeyCode::Home);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        assert!(o.settings.editing.is_none());
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[3].starts_with("│ › Private facts"), "{got:?}");
    }

    /// `zephyr.proposals` with held actions (made-up data): a delayed post, a message whose
    /// class asks about new recipients, a payment that needs a strong channel, and a setting.
    fn actions_answer() -> serde_json::Value {
        let due = NOW / 1000 + 600;
        serde_json::json!({"proposals": [
            {"id": 8, "kind": "run", "class": "posts", "status": "scheduled", "due_at": due,
             "summary": "gh issue create on owner/repo",
             "card": "[zephyr] delayed #8: post (GitHub)\ngh issue create on owner/repo\nsending at 14:12 unless /cancel 8",
             "age_secs": 60, "expires_in_secs": 86_340, "strong": false, "once": false},
            {"id": 7, "kind": "run", "class": "messages", "status": "pending",
             "summary": "email to alice@example.com",
             "card": "[zephyr] approve #7: message (email)\nto: alice@example.com (new) · subject: \"Invoice March\"\n/ok 7 · /ok 7 once · /no 7 · expires in 24 h",
             "age_secs": 180, "expires_in_secs": 86_220, "strong": false, "once": true},
            {"id": 6, "kind": "wait", "class": "money", "status": "pending",
             "summary": "pay on https://shop.example.com",
             "card": "[zephyr] approve #6: payment (browser)\n/ok 6 · /no 6 · expires in 10 min · this one needs tui or chat",
             "age_secs": 30, "expires_in_secs": 570, "strong": true, "once": false},
            {"id": 5, "card": "/set tasks.parallel 4", "kind": "setting", "key": "tasks.parallel",
             "age_secs": 7_200, "expires_in_secs": 79_200, "strong": false},
        ]})
    }

    fn keys_named(o: &Overlay) -> Vec<&'static str> {
        settings_panel::hints(o).into_iter().map(|h| h.0).collect()
    }

    #[test]
    fn an_action_card_approves_once_with_1() {
        let s = snap();
        let mut o = settings_panel();
        o.settings.load_waiting(&actions_answer());
        draw(&mut o, &s, 80, 20);
        to_waiting(&mut o, &s);
        // An action's row says what it is and what it does; a delayed one when it goes.
        let got = draw(&mut o, &s, 80, 20);
        let at = crate::settings::clock(NOW / 1000 + 600, &jiff::tz::TimeZone::UTC);
        assert!(
            got[1].contains("#8  post · gh issue create on owner/repo"),
            "{got:?}"
        );
        assert!(
            got[1].trim_end_matches(" │").ends_with(&format!("at {at}")),
            "{got:?}"
        );
        assert!(
            got[2].contains("#7  message · email to alice@example.com"),
            "{got:?}"
        );
        assert!(got[2].trim_end_matches(" │").ends_with("3m ago"), "{got:?}");
        assert!(got[3].contains("#6  payment · pay on"), "{got:?}");
        assert!(got[4].contains("#5  /set tasks.parallel 4"), "{got:?}");
        // A delayed action is cancelled with `c`.
        hit(&mut o, &s, KeyCode::Right);
        assert!(keys_named(&o).contains(&"c"), "{:?}", keys_named(&o));
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('c')),
            Action::Decide {
                id: 8,
                verb: "cancel".into()
            }
        );
        // The message: its whole card in the daemon's words; `1` approves it once.
        hit(&mut o, &s, KeyCode::Down);
        assert!(keys_named(&o).contains(&"1"), "{:?}", keys_named(&o));
        assert!(!keys_named(&o).contains(&"c"), "{:?}", keys_named(&o));
        hit(&mut o, &s, KeyCode::Enter);
        let card = draw(&mut o, &s, 80, 20);
        assert!(card[1].contains("held #7"), "{card:?}");
        assert!(
            card.iter()
                .any(|l| l.contains("│ approve #7: message (email)")),
            "{card:?}"
        );
        assert!(
            card.iter()
                .any(|l| l.contains("to: alice@example.com (new) · subject: \"Invoice March\"")),
            "{card:?}"
        );
        assert!(
            card.iter()
                .any(|l| l.contains("waited 3m · expires in 23h 57m")),
            "{card:?}"
        );
        assert_eq!(keys_named(&o), ["o", "1", "d", "esc"]);
        assert_eq!(hit(&mut o, &s, KeyCode::Char('c')), Action::None);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('1')),
            Action::Decide {
                id: 7,
                verb: "once".into()
            }
        );
        hit(&mut o, &s, KeyCode::Esc);
        // A payment's class does not ask about new recipients, so once is no different: `1`
        // approves it as `o` does, and is not offered.
        hit(&mut o, &s, KeyCode::Down);
        assert!(!keys_named(&o).contains(&"1"), "{:?}", keys_named(&o));
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("a payment · expires in 9m · needs a strong channel")),
            "{got:?}"
        );
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('1')),
            Action::Decide {
                id: 6,
                verb: "ok".into()
            }
        );
        // A setting's proposal is as it was: `1` is `once`, which the daemon reads as ok.
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('1')),
            Action::Decide {
                id: 5,
                verb: "once".into()
            }
        );
    }

    /// `1` on a message or a post sends `once` whatever the listed flag says: the mode may have
    /// moved to ask-new since the list came, and `ok` would then remember the recipient. It is
    /// offered only where the list says once differs.
    #[test]
    fn one_approves_a_message_or_post_once_whatever_its_listed_mode() {
        let s = snap();
        let mut o = settings_panel();
        let answer = serde_json::json!({"proposals": [
            {"id": 9, "kind": "run", "class": "posts", "status": "pending",
             "summary": "gh pr create on octo/hello", "card": "[zephyr] approve #9: post (GitHub)",
             "age_secs": 60, "expires_in_secs": 86_340, "strong": false, "once": false},
        ]});
        o.settings.load_waiting(&answer);
        to_waiting(&mut o, &s);
        hit(&mut o, &s, KeyCode::Right);
        assert!(!keys_named(&o).contains(&"1"), "{:?}", keys_named(&o));
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('1')),
            Action::Decide {
                id: 9,
                verb: "once".into()
            }
        );
    }

    /// A wrapped card line goes on indented, so text the agent or a page shaped cannot start a
    /// row where the daemon's labels start.
    #[test]
    fn a_wrapped_card_line_is_indented() {
        let s = snap();
        let mut o = settings_panel();
        let card = format!(
            "[zephyr] approve #4: message (email)\nfirst lines: \"{}to: bob@evil.example\"\n/ok 4 · /no 4",
            "x ".repeat(31)
        );
        o.settings.load_waiting(&serde_json::json!({"proposals": [
            {"id": 4, "kind": "run", "class": "messages", "status": "pending",
             "summary": "email to a@x.org", "card": card,
             "age_secs": 60, "expires_in_secs": 86_340, "strong": false, "once": false},
        ]}));
        to_waiting(&mut o, &s);
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Enter);
        let got = draw(&mut o, &s, 80, 20);
        let at = got
            .iter()
            .position(|l| l.contains("first lines: "))
            .unwrap();
        assert!(got[at + 1].starts_with("│   "), "{got:?}");
        assert!(got[at + 1].contains("to: bob@evil.example"), "{got:?}");
        assert!(
            !got.iter().any(|l| l.starts_with("│ to: ")),
            "a continuation reads as a label: {got:?}"
        );
        // The daemon's own lines still start at the edge.
        assert!(got[at + 2].starts_with("│ /ok 4 · /no 4"), "{got:?}");
    }

    /// The facts' problem is drawn apart from their rows, so the row the keyboard is on is the
    /// one kept in view.
    #[test]
    fn a_facts_problem_does_not_move_the_chosen_row() {
        use super::super::conn::FactEntry;
        let s = snap();
        let mut o = Overlay::new(Kind::Settings);
        o.settings.load(&approvals_answer(false));
        let fact = |name: &str| FactEntry {
            name: name.into(),
            kind: "text".into(),
            allow: vec![],
        };
        o.settings
            .load_facts(vec![fact("home-address"), fact("phone")]);
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Down);
        o.settings.facts_refused("no fact named gone");
        let focus: Vec<String> = (list(&o, &s).into_iter())
            .filter_map(|r| match r {
                Row::Focus(t) => Some(rpi_tui::strip_ansi(&t)),
                _ => None,
            })
            .collect();
        assert_eq!(focus.len(), 1, "{focus:?}");
        assert!(focus[0].contains("› phone"), "{focus:?}");
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter().any(|l| l.contains("no fact named gone")),
            "{got:?}"
        );
    }

    #[test]
    fn the_fact_value_is_masked_while_typed() {
        use super::super::conn::FactEntry;
        // Made up for the test; nothing here is anyone's.
        const VALUE: &str = "135798";
        let s = snap();
        let mut o = Overlay::new(Kind::Settings);
        o.settings.load(&approvals_answer(false));
        o.settings.load_facts(vec![FactEntry {
            name: "home-address".into(),
            kind: "text".into(),
            allow: vec!["https://shop.example.com".into()],
        }]);
        hit(&mut o, &s, KeyCode::Down);
        hit(&mut o, &s, KeyCode::Down);
        let got = draw(&mut o, &s, 80, 20);
        assert!(got[3].starts_with("│ › Private facts"), "{got:?}");
        assert!(
            line_of(&got, "home-address").contains("text · allow: https://shop.example.com"),
            "{got:?}"
        );
        assert!(keys_named(&o).contains(&"a"), "{:?}", keys_named(&o));
        // `a` adds one: a name, a kind (space changes it), the value masked, where it may go.
        assert_eq!(hit(&mut o, &s, KeyCode::Char('a')), Action::None);
        typing(&mut o, &s, "test-pin");
        hit(&mut o, &s, KeyCode::Tab);
        hit(&mut o, &s, KeyCode::Char(' '));
        hit(&mut o, &s, KeyCode::Tab);
        typing(&mut o, &s, &VALUE[..5]);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter().any(|l| l.contains("add a private fact")),
            "{got:?}"
        );
        assert!(line_of(&got, "kind   ").contains("digits"), "{got:?}");
        assert!(line_of(&got, "› value").contains("•••••"), "{got:?}");
        let shown = format!("{got:?}{o:?}");
        assert!(!shown.contains(&VALUE[..5]), "{shown}");
        // Too short for a digits fact: the form stays open with the reason, the value kept.
        hit(&mut o, &s, KeyCode::Tab);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter().any(|l| l.contains("needs at least 6 digits")),
            "{got:?}"
        );
        hit(&mut o, &s, KeyCode::BackTab);
        typing(&mut o, &s, &VALUE[5..]);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        let saved = hit(&mut o, &s, KeyCode::Enter);
        let shown = format!("{saved:?}");
        assert!(!shown.contains(VALUE), "{shown}");
        assert!(shown.contains("value: 6"), "{shown}");
        assert_eq!(
            saved,
            Action::FactsPut(Box::new(crate::facts::Fact {
                name: "test-pin".into(),
                kind: crate::facts::FactKind::Digits,
                value: VALUE.into(),
                allow: vec![],
            }))
        );
        // The value leaves with the save: nothing on screen or in the panel holds it.
        assert!(o.settings.fact_form.is_none());
        let shown = format!("{:?}{o:?}", draw(&mut o, &s, 80, 20));
        assert!(!shown.contains(VALUE), "{shown}");
        // A name in use is edited, not added again; an edit keeps the name and, left empty,
        // the saved value.
        hit(&mut o, &s, KeyCode::Right);
        hit(&mut o, &s, KeyCode::Char('a'));
        typing(&mut o, &s, "home-address");
        hit(&mut o, &s, KeyCode::Tab);
        hit(&mut o, &s, KeyCode::Tab);
        typing(&mut o, &s, "made-up place");
        hit(&mut o, &s, KeyCode::Tab);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("home-address is saved; e edits it")),
            "{got:?}"
        );
        hit(&mut o, &s, KeyCode::Esc);
        assert_eq!(hit(&mut o, &s, KeyCode::Char('e')), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter().any(|l| l.contains("edit home-address")),
            "{got:?}"
        );
        assert!(
            got.iter()
                .any(|l| l.contains("an empty value keeps the saved one")),
            "{got:?}"
        );
        // The name cannot be typed over.
        hit(&mut o, &s, KeyCode::BackTab);
        hit(&mut o, &s, KeyCode::BackTab);
        typing(&mut o, &s, "x");
        let got = draw(&mut o, &s, 80, 20);
        let name = line_of(&got, "› name");
        assert!(name.contains("home-address") && !name.contains("home-addressx"));
        hit(&mut o, &s, KeyCode::Tab);
        hit(&mut o, &s, KeyCode::Tab);
        assert_eq!(hit(&mut o, &s, KeyCode::Enter), Action::None);
        let Action::FactsPut(kept) = hit(&mut o, &s, KeyCode::Enter) else {
            panic!("no save")
        };
        assert_eq!(
            (kept.name.as_str(), kept.value.as_str(), &kept.allow),
            (
                "home-address",
                "",
                &vec!["https://shop.example.com".to_string()]
            )
        );
        // `d` asks first; `y` deletes.
        assert_eq!(hit(&mut o, &s, KeyCode::Char('d')), Action::None);
        let got = draw(&mut o, &s, 80, 20);
        assert!(
            got.iter()
                .any(|l| l.contains("delete the private fact home-address? y deletes it")),
            "{got:?}"
        );
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('y')),
            Action::FactsRm("home-address".into())
        );
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
        let commands = COMMANDS.iter().filter(|c| c.1.starts_with('/')).count();
        assert_eq!(usages("/").len(), commands);
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
                items: slash("/rule"),
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
        // Naming a command completes it; typing its arguments leaves the text alone.
        let taken = |t: &str| -> Vec<(String, bool)> {
            (slash(t).into_iter())
                .map(|c| (c.insert, c.then_run))
                .collect()
        };
        assert_eq!(
            taken("/rule"),
            [
                ("/rules".into(), true),
                ("/rule ".into(), false),
                ("/rule rm ".into(), false)
            ]
        );
        assert_eq!(taken("/rule x"), [("/rule x".into(), false)]);
    }

    #[test]
    fn arguments_complete_from_what_the_tui_holds() {
        let mut s = State {
            snapshot: snap(),
            vault: vec!["github".into(), "gitlab".into(), "npm".into()],
            ..Default::default()
        };
        let got = |t: &str, s: &State| -> Vec<(String, String, String)> {
            (args(t, s, 3).into_iter())
                .map(|c| (c.label, c.detail, c.insert))
                .collect()
        };
        let one = |l: &str, d: &str, i: &str| (l.to_string(), d.to_string(), i.to_string());
        // Before `zephyr.models` answered: each account's model.
        assert_eq!(
            got("/model gpt", &s),
            [one("openai/gpt-6", "spare", "/model openai/gpt-6")]
        );
        s.models = models();
        assert_eq!(
            got("/model SONN", &s),
            [one(
                "anthropic/claude-sonnet-5",
                "work",
                "/model anthropic/claude-sonnet-5"
            )]
        );
        assert_eq!(got("/copy ", &s).len(), 3);
        assert_eq!(
            got("/job run ", &s),
            [one("3", "renew domain", "/job run 3")]
        );
        assert_eq!(got("/job rm 4", &s), []);
        let names: Vec<String> = got("/vault rm git", &s).into_iter().map(|c| c.0).collect();
        assert_eq!(names, ["github", "gitlab"]);
        assert_eq!(got("/vault revoke npm", &s).len(), 1);
        assert!(args("/vault revoke npm x", &s, 3).is_empty());
        s.sessions = vec![
            super::super::conn::SessionInfo {
                id: "main".into(),
                name: None,
                at: 1,
            },
            super::super::conn::SessionInfo {
                id: "main-9".into(),
                name: Some("taxes".into()),
                at: 2,
            },
        ];
        assert_eq!(
            got("/switch main-", &s),
            [one("main-9", "taxes", "/switch main-9")]
        );
        // A `/rename` name finds its session.
        assert_eq!(
            got("/switch tax", &s),
            [one("main-9", "taxes", "/switch main-9")]
        );
        assert!(args("/rule x", &s, 3).is_empty() && args("/model", &s, 3).is_empty());
        assert!(args("/model a\nb", &s, 3).is_empty());
        assert!(args("/copy 1", &s, 3).iter().all(|c| c.then_run));

        let saved = [
            ("hi".to_string(), "Say hi".to_string()),
            ("review".to_string(), "Review $1".to_string()),
        ];
        let p: Vec<(String, String)> = (prompt_args("/p r", &saved).into_iter())
            .map(|c| (c.label, c.insert))
            .collect();
        assert_eq!(p, [("review".to_string(), "/p review".to_string())]);
        assert_eq!(prompt_args("/p ", &saved).len(), 2);
        assert!(prompt_args("/p review x", &saved).is_empty());
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

    #[test]
    fn an_oauth_sign_in_that_takes_a_line_still_points_at_the_browser() {
        // Under the network floor an OAuth login is `need_input`: the browser first, the paste
        // only if its last page cannot open. A setup token has a URL too, and wants its code.
        let oauth = phase_text(&waiting("need_input"), true);
        assert!(
            oauth.contains("browser") && oauth.contains("paste its address"),
            "{oauth}"
        );
        let token = phase_text(&waiting("need_input"), false);
        assert_eq!(token, "waiting for the code it asks for");
        let browser = "waiting for you to finish in the browser";
        assert_eq!(phase_text(&waiting("waiting"), true), browser);
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

    fn activity_answer() -> serde_json::Value {
        serde_json::json!({"since": 1, "entries": [
            {"at": 3, "kind": "out", "subject": "main", "status": "started",
             "text": "curl POST api.example.com", "line": "14:02 out main: curl POST api.example.com",
             "stream": true},
            {"at": 2, "kind": "tool", "subject": "main", "status": "started", "text": "read {}",
             "line": "14:01 tool main: read {}", "stream": true},
            {"at": 1, "kind": "task", "subject": "#12", "status": "completed",
             "text": "summarise the inbox", "line": "13:40 task #12 completed: summarise the inbox",
             "stream": false},
        ], "next": "5.audit.9"})
    }

    fn grants_answer() -> serde_json::Value {
        serde_json::json!({"since": null, "entries": [
            {"at": 1, "kind": "grant", "subject": "messages",
             "line": "messages bob@x.org, known since 11-02 · /approvals forget 4",
             "revoke": "/approvals forget 4"},
            {"at": 2, "kind": "grant", "subject": "#3",
             "line": "job #3 morning-brief, 0 7 * * *, to discord · /job rm 3", "revoke": "/job rm 3"},
            {"at": 3, "kind": "grant", "subject": "k",
             "line": "k until 18:00, then [vault:A] · set k back by hand", "revoke": null},
        ], "next": null})
    }

    #[test]
    fn activity_mutes_tool_rows_and_asks_for_the_next_page_at_the_bottom() {
        let s = snap();
        let mut o = settings_panel();
        o.settings.load_activity(&activity_answer(), false, 0);
        // Activity and Grants follow Waiting.
        hit(&mut o, &s, KeyCode::End);
        hit(&mut o, &s, KeyCode::Up);
        let got = draw(&mut o, &s, 100, 20);
        assert!(got[5].starts_with("│   Waiting"), "{got:?}");
        assert!(got[6].starts_with("│ › Activity"), "{got:?}");
        assert!(got[7].starts_with("│   Grants"), "{got:?}");
        for (i, line) in [
            "14:02 out main: curl POST api.example.com",
            "14:01 tool main: read {}",
            "13:40 task #12 completed: summarise the inbox",
        ]
        .iter()
        .enumerate()
        {
            assert!(got[i + 1].contains(line), "{got:?}");
        }
        let rows = list(&o, &s);
        let raw = |text: &str| {
            (rows.iter())
                .find_map(|r| match r {
                    Row::Line(l) | Row::Focus(l) if l.contains(text) => Some(l.clone()),
                    _ => None,
                })
                .unwrap()
        };
        assert!(raw("read {}").contains(&DARK.muted.fg("14:01 tool main: read {}")));
        assert!(!raw("curl POST").contains(&DARK.muted.fg("14:02 out main: curl POST")));
        // Into the rows: reaching the last asks once for the page before the cursor.
        hit(&mut o, &s, KeyCode::Right);
        assert_eq!(
            hit(&mut o, &s, KeyCode::End),
            Action::ActivityPage("5.audit.9".into())
        );
        assert_eq!(hit(&mut o, &s, KeyCode::End), Action::None);
        hit(&mut o, &s, KeyCode::Up);
        assert_eq!(hit(&mut o, &s, KeyCode::Down), Action::None, "asked once");
        let mut page2 = activity_answer();
        page2["entries"] = serde_json::json!([{"at": 0, "kind": "job", "subject": "#3 run 88",
            "status": "completed", "text": "morning-brief",
            "line": "13:00 job #3 run 88 completed: morning-brief", "stream": false}]);
        page2["next"] = serde_json::Value::Null;
        o.settings.load_activity(&page2, true, 0);
        let got = draw(&mut o, &s, 100, 20);
        assert!(
            got[4].contains("13:00 job #3 run 88 completed: morning-brief"),
            "{got:?}"
        );
        assert!(got[1].contains("14:02 out main"), "{got:?}");
        assert_eq!(
            hit(&mut o, &s, KeyCode::End),
            Action::None,
            "no page after the last"
        );
    }

    #[test]
    fn r_on_a_grant_sends_its_revoke() {
        let s = snap();
        let mut o = settings_panel();
        o.settings.load_grants(&grants_answer(), 0);
        hit(&mut o, &s, KeyCode::End);
        let got = draw(&mut o, &s, 100, 20);
        assert!(got[7].starts_with("│ › Grants"), "{got:?}");
        assert!(
            got[1].contains("messages bob@x.org, known since 11-02 · "),
            "{got:?}"
        );
        assert!(got[2].contains("job #3 morning-brief"), "{got:?}");
        hit(&mut o, &s, KeyCode::Right);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('r')),
            Action::Revoke("/approvals forget 4".into())
        );
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Char('r')),
            Action::Revoke("/job rm 3".into())
        );
        // A grant whose value could not be shown as it is offers no command to send.
        hit(&mut o, &s, KeyCode::Down);
        assert_eq!(hit(&mut o, &s, KeyCode::Char('r')), Action::None);
        let hints = settings_panel::hints(&o);
        assert!(hints.iter().any(|(k, _)| *k == "r"), "{hints:?}");
        hit(&mut o, &s, KeyCode::Char('?'));
        let help = draw(&mut o, &s, 100, 30);
        assert!(
            help.iter()
                .any(|l| l.contains("r") && l.contains("Grants: take it back")),
            "{help:?}"
        );
    }

    #[test]
    fn an_old_daemon_gives_an_empty_activity_tab() {
        let s = snap();
        let mut o = settings_panel();
        let old = serde_json::json!({"entries": [], "next": null, "old": true});
        o.settings.load_activity(&old, false, 0);
        o.settings.load_grants(&old, 0);
        hit(&mut o, &s, KeyCode::End);
        hit(&mut o, &s, KeyCode::Up);
        let got = draw(&mut o, &s, 100, 20);
        assert!(
            got[1].contains("this zephyr keeps no activity record; zephyr upgrade"),
            "{got:?}"
        );
        assert_eq!(hit(&mut o, &s, KeyCode::Right), Action::None);
        hit(&mut o, &s, KeyCode::Down);
        let got = draw(&mut o, &s, 100, 20);
        assert!(
            got[1].contains("this zephyr keeps no activity record; zephyr upgrade"),
            "{got:?}"
        );
    }

    /// PR #39 review: a failed page keeps the cursor of the page it asked for, so reaching the
    /// bottom again asks again; an answer to a panel opened before this one is dropped.
    #[test]
    fn a_failed_or_stale_activity_page_never_loses_or_mixes_rows() {
        let s = snap();
        let mut o = settings_panel();
        o.settings.generation = 7;
        o.settings.load_activity(&activity_answer(), false, 7);
        hit(&mut o, &s, KeyCode::End);
        hit(&mut o, &s, KeyCode::Up);
        hit(&mut o, &s, KeyCode::Right);
        assert_eq!(
            hit(&mut o, &s, KeyCode::End),
            Action::ActivityPage("5.audit.9".into())
        );
        o.settings.load_activity(
            &serde_json::json!({"error": "the gateway is busy"}),
            true,
            7,
        );
        hit(&mut o, &s, KeyCode::Up);
        assert_eq!(
            hit(&mut o, &s, KeyCode::Down),
            Action::ActivityPage("5.audit.9".into()),
            "asked again after the failure"
        );
        let got = draw(&mut o, &s, 100, 20);
        assert!(
            got.iter().any(|l| l.contains("the gateway is busy")),
            "{got:?}"
        );
        // A page from the panel opened before this one is not this panel's.
        let mut stale = activity_answer();
        stale["entries"] = serde_json::json!([{"at": 0, "kind": "job", "subject": "#9",
            "status": "completed", "text": "old", "line": "09:00 job #9 run 1 completed: old",
            "stream": false}]);
        o.settings.load_activity(&stale, true, 6);
        o.settings.load_grants(&grants_answer(), 6);
        let got = draw(&mut o, &s, 100, 20);
        assert!(!got.iter().any(|l| l.contains("job #9 run 1")), "{got:?}");
        assert_eq!(o.settings.grants, None, "{got:?}");
    }
}
