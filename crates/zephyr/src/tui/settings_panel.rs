//! The settings panel (`/settings`, settings spec §4.6): the daemon's settings by section on the
//! right of a section list, an editor that fits each kind, the owner's private facts with a
//! masked field for their values (approvals spec §4.5), and a Waiting tab for the proposals and
//! held actions the owner has to decide. overlays.rs draws it with [`rows`] and hands it its
//! keys with [`key`]; everything it shows comes from `zephyr.settings`, `zephyr.facts_list` and
//! `zephyr.proposals`.

use super::conn::{FactEntry, Snapshot};
use super::overlays::{
    column, field_rows, fields_debug, model_groups, picks, Action, Overlay, Row,
};
use super::theme::{glyph, Palette};
use super::view::clean;
use crate::facts::{Fact, FactKind};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_tui::{bold, truncate_to_width, visible_width, CURSOR_MARKER};
use serde::Deserialize;
use serde_json::Value;

/// One section of `zephyr.settings`.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Section {
    pub name: String,
    pub settings: Vec<Entry>,
}

/// One setting as `zephyr.settings` lists it (DESIGN §4.3.1).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub key: String,
    pub label: String,
    pub help: String,
    /// `{"type": …}` with its bounds or choices.
    pub kind: Value,
    /// As replies show it; `None` is unset.
    pub value: Option<String>,
    /// `default`, `you`, `agent (#N)` or `until HH:MM`.
    pub source: String,
    /// The agent's access (`never`, `propose`, `tighten`), or `read-only`.
    pub access: String,
    pub strong: bool,
    /// While a time limit runs: when it ends, unix seconds.
    pub until: Option<i64>,
    /// The approvals preset: a hand-set class differs from it (`semi (modified)`).
    pub modified: bool,
}

impl Entry {
    fn read_only(&self) -> bool {
        self.access == "read-only"
    }

    /// The value as a row shows it: `none` when unset, the preset with `(modified)`.
    fn shown(&self) -> String {
        let value = self.value.as_deref().unwrap_or("none");
        match self.modified {
            true => format!("{value} (modified)"),
            false => value.to_string(),
        }
    }

    fn bool(&self) -> bool {
        self.kind["type"] == "bool"
    }
}

/// One proposal as `zephyr.proposals` lists it, newest first.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default)]
pub struct Proposal {
    pub id: i64,
    /// What `/ok` runs, in the daemon's words: for a held action, the whole card the chat got.
    pub card: String,
    /// `setting` | `command`, or a held action's `run` | `pass` | `wait`.
    pub kind: String,
    pub key: Option<String>,
    pub age_secs: i64,
    pub expires_in_secs: i64,
    /// Only a strong channel may approve it: it loosens a protected setting, or it is a payment
    /// while `approvals.strong_only` names money.
    pub strong: bool,
    /// A held action's class: `messages`, `posts`, `money`, or `outbound` (held for a fact).
    pub class: Option<String>,
    /// A held action's summary, in the gate's words.
    pub summary: Option<String>,
    /// `pending`, or `scheduled` for an action waiting out its delay.
    pub status: String,
    /// When a `scheduled` action goes, unix seconds.
    pub due_at: Option<i64>,
    /// `/ok N once` differs from `/ok N` for this action: its class asks only about new
    /// targets, and once approves it without remembering them.
    pub once: bool,
}

impl Proposal {
    /// A held message, post or payment, not a command or a setting.
    fn action(&self) -> bool {
        matches!(self.kind.as_str(), "run" | "pass" | "wait")
    }

    /// An action waiting out its delay: `c` cancels it before it goes.
    fn delayed(&self) -> bool {
        self.action() && self.status == "scheduled"
    }

    /// Whether `1` approves it once: always for a command or a setting (where the daemon reads
    /// `once` as `ok`), and for an action only where once differs from ok.
    fn offers_once(&self) -> bool {
        !self.action() || self.once
    }

    /// What a held action is, with its article: `a message`, `a payment`.
    fn what(&self) -> (&'static str, &'static str) {
        match self.class.as_deref() {
            Some("messages") => ("a", "message"),
            Some("posts") => ("a", "post"),
            Some("money") => ("a", "payment"),
            _ => ("an", "outgoing call"),
        }
    }
}

/// The daemon's section the private facts follow on the left: a daemon with approvals keeps
/// facts too, and one without has neither.
const APPROVALS: &str = "Approvals";

/// What the left column lists: the daemon's sections, the private facts, then Waiting.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Tab {
    Section(usize),
    Facts,
    Waiting,
}

/// The facts form's fields, in order.
const NAME: usize = 0;
const KIND: usize = 1;
const VALUE: usize = 2;
const ALLOW: usize = 3;
const KINDS: [&str; 3] = ["text", "digits", "email"];

/// The form for a private fact: its name, its kind, its value, and where it may go without a
/// check. The value is drawn one bullet per character ([`field_rows`]), handed over in the
/// `facts_put` the moment the form is saved, and dropped with the form: never shown, logged
/// (`Debug` counts it) or kept.
#[derive(Clone, PartialEq)]
pub struct FactForm {
    /// Editing a saved fact: its name stays, and an empty value keeps the saved one.
    pub editing: bool,
    /// The field being typed into.
    pub at: usize,
    /// `(label, secret, text)` per field.
    pub fields: Vec<(&'static str, bool, String)>,
}

impl std::fmt::Debug for FactForm {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FactForm")
            .field("editing", &self.editing)
            .field("at", &self.at)
            .field("fields", &fields_debug(&self.fields))
            .finish()
    }
}

impl FactForm {
    fn new() -> FactForm {
        FactForm {
            editing: false,
            at: NAME,
            fields: vec![
                ("name", false, String::new()),
                ("kind", false, KINDS[0].into()),
                ("value", true, String::new()),
                ("may go to", false, String::new()),
            ],
        }
    }

    /// The form to edit `f`, on its value: everything but the value, which an empty field keeps.
    fn edit(f: &FactEntry) -> FactForm {
        let mut form = FactForm::new();
        form.editing = true;
        form.fields[NAME].2 = f.name.clone();
        form.fields[KIND].2 = f.kind.clone();
        form.fields[ALLOW].2 = f.allow.join(", ");
        form.at = VALUE;
        form
    }

    /// Whether typing goes into the field at the cursor: not a saved fact's name, and the
    /// kind only turns.
    fn typed(&self) -> bool {
        !(self.at == KIND || (self.editing && self.at == NAME))
    }

    fn type_text(&mut self, text: &str) {
        if self.typed() {
            let one_line = text.replace(['\r', '\n'], "");
            self.fields[self.at].2.push_str(&one_line);
        }
    }

    fn backspace(&mut self) {
        if self.typed() {
            self.fields[self.at].2.pop();
        }
    }

    /// The next kind (`by` 1) or the one before (-1).
    fn turn_kind(&mut self, by: isize) {
        let now = KINDS.iter().position(|k| *k == self.fields[KIND].2);
        let next = (now.unwrap_or(0) as isize + by).rem_euclid(KINDS.len() as isize);
        self.fields[KIND].2 = KINDS[next as usize].into();
    }

    /// The fact to save; the targets are comma separated.
    fn fact(&self) -> Fact {
        let field = |i: usize| self.fields[i].2.trim().to_string();
        let allow = (self.fields[ALLOW].2.split(','))
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(String::from)
            .collect();
        Fact {
            name: field(NAME),
            kind: FactKind::parse(&field(KIND)).unwrap_or_default(),
            value: field(VALUE),
            allow,
        }
    }
}

/// `Kind::Settings`' own state.
#[derive(Debug, Clone, Default)]
pub struct SettingsPanel {
    /// `zephyr.settings`' answer; `None` until it came.
    pub sections: Option<Vec<Section>>,
    /// The channels that may loosen a protected setting.
    pub strong: Vec<String>,
    /// Why the daemon would not list them.
    pub problem: Option<String>,
    /// The open section: an index into `sections`, one past the last for Waiting.
    pub at: usize,
    /// The row chosen in it while the keyboard is on the right; `None` on the sections.
    pub row: Option<usize>,
    /// The edit in progress.
    pub editing: Option<Editor>,
    /// `zephyr.proposals`' answer, newest first; `None` until it came.
    pub waiting: Option<Vec<Proposal>>,
    /// The proposal whose whole card is open.
    pub card: Option<i64>,
    /// The last answer, where the owner is looking: whether it went through, and what it said.
    pub note: Option<(bool, String)>,
    /// `?`: the panel's keys instead of the settings.
    pub help: bool,
    /// The proposal the Waiting cursor was on, when a reload took it away and moved the cursor
    /// to another the owner has not seen chosen: the next `o`, `1`, `c` or `d` only says so.
    moved: Option<i64>,
    /// `zephyr.facts_list`'s answer: names, kinds and allowed targets, never a value; `None`
    /// until it came.
    pub facts: Option<Vec<FactEntry>>,
    /// Why the daemon would not list or change the facts.
    pub facts_problem: Option<String>,
    /// The private fact being added or edited.
    pub fact_form: Option<FactForm>,
    /// The fact `d` asked to delete, until `y` or any other key.
    pub fact_confirm: Option<String>,
    /// The fact the last facts call saved (`true`) or deleted, for the note its answer brings.
    fact_sent: Option<(bool, String)>,
}

/// An edit of one setting, in the editor its kind takes.
#[derive(Debug, Clone, PartialEq)]
pub struct Editor {
    pub key: String,
    pub label: String,
    pub help: String,
    pub kind: Value,
    /// The value in effect, marked in a list.
    pub value: Option<String>,
    /// What is typed into a field.
    pub text: String,
    /// The time limit, once `t` or Tab asked for one: `30m`, `2h`, `1d`.
    pub until: Option<String>,
    /// The keyboard is on the time limit.
    pub on_until: bool,
    /// Sent; waiting for `zephyr.settings_set`'s answer.
    pub sent: bool,
}

/// The editor a kind takes.
#[derive(Debug, Clone, PartialEq)]
enum Field {
    /// Pick one value: true or false, or a choice.
    List(Vec<String>),
    /// The model picker; `optional` offers `none` too.
    Model { optional: bool },
    /// A typed value.
    Typed,
}

fn field(kind: &Value) -> Field {
    match kind["type"].as_str().unwrap_or_default() {
        "bool" => Field::List(vec!["true".into(), "false".into()]),
        "choice" => Field::List(options(kind)),
        "model" => Field::Model { optional: false },
        "optional" => match field(&kind["inner"]) {
            Field::List(mut o) => {
                o.push("none".into());
                Field::List(o)
            }
            Field::Model { .. } => Field::Model { optional: true },
            Field::Typed => Field::Typed,
        },
        _ => Field::Typed,
    }
}

fn options(kind: &Value) -> Vec<String> {
    (kind["options"].as_array().into_iter().flatten())
        .filter_map(|o| o.as_str().map(line))
        .collect()
}

/// The word that clears an optional setting: an approval class's goes back to the preset, so it
/// reads `default` (which the daemon takes, as it takes `none`) and never looks like `off`.
fn clearing(key: &str) -> &'static str {
    match key.starts_with("approvals.") {
        true => "default",
        false => "none",
    }
}

/// What a kind takes, in words: `a whole number, 1 to 32`; `clear` is the word that unsets it.
fn takes(kind: &Value, clear: &str) -> String {
    let num = |v: &Value| match v.as_f64() {
        Some(f) if f.fract() == 0.0 && f.abs() < 1e15 => format!("{}", f as i64),
        Some(f) => format!("{f}"),
        None => "?".into(),
    };
    match kind["type"].as_str().unwrap_or_default() {
        t @ ("int" | "float") => {
            let what = if t == "int" {
                "a whole number"
            } else {
                "a number"
            };
            match kind["max"].as_f64() {
                // A bound nobody reaches is no bound worth reading.
                Some(max) if max < 1e9 => {
                    format!("{what}, {} to {}", num(&kind["min"]), num(&kind["max"]))
                }
                _ => format!("{what} from {}", num(&kind["min"])),
            }
        }
        "bool" => "true or false".into(),
        "duration" => "a time like 30m, 2h or 1d".into(),
        "choice" => format!("one of {}", options(kind).join(", ")),
        "model" => "PROVIDER/MODEL, one an account lists".into(),
        "list" => format!("names, comma separated: {}", options(kind).join(", ")),
        "mode" => format!("one of {}", options(kind).join(", ")),
        "optional" if clear == "default" => {
            format!(
                "{}, or default (the preset's)",
                takes(&kind["inner"], clear)
            )
        }
        "optional" => format!("{}, or {clear}", takes(&kind["inner"], clear)),
        _ => match kind["max"].as_u64() {
            Some(max) if max > 0 => format!("text, at most {max} characters"),
            _ => "text".into(),
        },
    }
}

impl SettingsPanel {
    /// Takes `zephyr.settings`' answer, or the error a daemon without it gave.
    pub fn load(&mut self, v: &Value) {
        if let Some(e) = v.get("error") {
            self.problem = Some(format!(
                "the gateway did not list its settings: {}",
                line(e.as_str().unwrap_or("no answer"))
            ));
            self.sections.get_or_insert_with(Vec::new);
        } else {
            let mut sections: Vec<Section> =
                serde_json::from_value(v["sections"].clone()).unwrap_or_default();
            for x in &mut sections {
                x.name = line(&x.name);
                for e in &mut x.settings {
                    for text in [
                        &mut e.key,
                        &mut e.label,
                        &mut e.help,
                        &mut e.source,
                        &mut e.access,
                    ] {
                        *text = line(text);
                    }
                    e.value = e.value.as_deref().map(line);
                }
            }
            self.sections = Some(sections);
            let strong: Vec<String> =
                serde_json::from_value(v["strong_channels"].clone()).unwrap_or_default();
            self.strong = strong.iter().map(|c| line(c)).collect();
            self.problem = None;
        }
        self.clamp();
    }

    /// Takes `zephyr.proposals`' answer. The cursor stays on the proposal it was on wherever the
    /// newest-first list moved it; if that one is gone, the nearest row is chosen and marked
    /// unseen. A card whose proposal is gone closes.
    pub fn load_waiting(&mut self, v: &Value) {
        let mut list: Vec<Proposal> =
            serde_json::from_value(v["proposals"].clone()).unwrap_or_default();
        for pr in &mut list {
            pr.card = tidy(&pr.card);
            pr.kind = line(&pr.kind);
            pr.key = pr.key.as_deref().map(line);
            pr.class = pr.class.as_deref().map(line);
            pr.summary = pr.summary.as_deref().map(line);
            pr.status = line(&pr.status);
        }
        let picked = self.proposal().map(|pr| pr.id);
        if self.card.is_some_and(|id| !list.iter().any(|p| p.id == id)) {
            self.card = None;
        }
        let at = picked.and_then(|id| list.iter().position(|p| p.id == id));
        self.waiting = Some(list);
        match (picked, at) {
            (Some(_), Some(i)) => self.row = Some(i),
            (Some(id), None) => self.moved = Some(id),
            (None, _) => {}
        }
        self.clamp();
        if self.proposal().is_none() {
            self.moved = None;
        }
    }

    /// Takes `zephyr.settings_set`'s answer: a change closes the editor and says what changed; a
    /// refusal keeps it open, with the reason under what was typed. Only the editor that sent
    /// the change is closed: one the owner opened since, after leaving that one, stays.
    pub fn answered(&mut self, v: &Value, tz: &jiff::tz::TimeZone) {
        let shown = |k: &str| line(v[k].as_str().unwrap_or("none"));
        let sent = self.editing.as_ref().is_some_and(|e| e.sent);
        match v["outcome"].as_str() {
            Some("changed") => {
                let mut said = format!(
                    "{} {} {} {}",
                    line(v["key"].as_str().unwrap_or_default()),
                    shown("old"),
                    glyph("→"),
                    shown("new")
                );
                if let Some(at) = v["until"].as_i64() {
                    said.push_str(&format!(" until {}", crate::settings::clock(at, tz)));
                }
                // The reload's own answer, unless it is the plain "applied".
                let applied = v["applied"].as_str().map(unprefixed).unwrap_or_default();
                if !applied.is_empty() && applied != "applied" {
                    said.push('\n');
                    said.push_str(&applied);
                }
                if sent {
                    self.editing = None;
                }
                self.note = Some((true, said));
            }
            Some("proposed") => {
                if sent {
                    self.editing = None;
                }
                self.note = Some((true, format!("proposed as #{}", v["id"])));
            }
            _ => {
                if let Some(e) = self.editing.as_mut().filter(|e| e.sent) {
                    e.sent = false;
                }
                let why = v["reason"].as_str().unwrap_or("refused");
                self.note = Some((false, unprefixed(why)));
            }
        }
    }

    /// Takes `zephyr.decide`'s answer: the card closes and the daemon's reply shows.
    pub fn decided(&mut self, v: &Value) {
        let ok = v["ok"].as_bool().unwrap_or(false);
        self.card = None;
        self.note = Some((ok, unprefixed(v["text"].as_str().unwrap_or_default())));
    }

    /// Takes the facts a `zephyr.facts_*` call answered with. Returns whether it answered a
    /// change made here, which then says what it did.
    pub fn load_facts(&mut self, list: Vec<FactEntry>) -> bool {
        let list = (list.into_iter())
            .map(|f| FactEntry {
                name: line(&f.name),
                kind: line(&f.kind),
                allow: f.allow.iter().map(|t| line(t)).collect(),
            })
            .collect();
        self.facts = Some(list);
        self.facts_problem = None;
        let sent = self.fact_sent.take();
        if let Some((saved, name)) = &sent {
            self.note = Some(match saved {
                true => (
                    true,
                    format!("saved the private fact {name}; its value is not shown"),
                ),
                false => (true, format!("deleted the private fact {name}")),
            });
        }
        self.clamp();
        sent.is_some()
    }

    /// Takes a facts call's refusal: why, where the facts are listed.
    pub fn facts_refused(&mut self, why: &str) {
        self.fact_sent = None;
        self.facts.get_or_insert_with(Vec::new);
        self.facts_problem = Some(unprefixed(why).replace('\n', " "));
    }

    /// The left column: each section, the private facts after Approvals, then Waiting.
    fn tabs(&self) -> Vec<Tab> {
        let mut out = vec![];
        for (i, x) in self.sections.iter().flatten().enumerate() {
            out.push(Tab::Section(i));
            if x.name == APPROVALS {
                out.push(Tab::Facts);
            }
        }
        out.push(Tab::Waiting);
        out
    }

    /// The open entry of the left column.
    fn tab(&self) -> Tab {
        self.tabs().get(self.at).copied().unwrap_or(Tab::Waiting)
    }

    fn waiting_tab(&self) -> bool {
        self.tab() == Tab::Waiting
    }

    /// The rows the open section has.
    fn len(&self) -> usize {
        match self.tab() {
            Tab::Waiting => self.waiting.as_ref().map_or(0, Vec::len),
            Tab::Facts => self.facts.as_ref().map_or(0, Vec::len),
            Tab::Section(i) => (self.sections.iter().flatten())
                .nth(i)
                .map_or(0, |x| x.settings.len()),
        }
    }

    fn entry(&self) -> Option<&Entry> {
        let Tab::Section(i) = self.tab() else {
            return None;
        };
        let section = self.sections.as_ref()?.get(i)?;
        section.settings.get(self.row?)
    }

    fn proposal(&self) -> Option<&Proposal> {
        match self.waiting_tab() {
            true => self.waiting.as_ref()?.get(self.row?),
            false => None,
        }
    }

    /// The fact the cursor is on.
    fn fact(&self) -> Option<&FactEntry> {
        match self.tab() {
            Tab::Facts => self.facts.as_ref()?.get(self.row?),
            _ => None,
        }
    }

    /// Keeps the selection on something that is still there after a new answer.
    fn clamp(&mut self) {
        self.at = self.at.min(self.tabs().len() - 1);
        let len = self.len();
        self.row = self
            .row
            .and_then(|r| len.checked_sub(1).map(|last| r.min(last)));
    }
}

/// Text from the daemon, safe to draw: [`clean`]'s cleaning (no escape sequences or control
/// characters), and no characters that turn the direction of what follows, so a value the agent
/// once had approved cannot paint over the panel or pass for other text. Lines are kept.
fn tidy(text: &str) -> String {
    // The embeddings, overrides and isolates the plugin refuses in rule text, and the marks.
    let turns = |c: char| {
        matches!(c,
            '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}'
            | '\u{200e}' | '\u{200f}' | '\u{061c}')
    };
    clean(text).chars().filter(|&c| !turns(c)).collect()
}

/// [`tidy`] on one line: a label, a value, a source.
fn line(text: &str) -> String {
    tidy(text).replace('\n', " ")
}

/// The daemon's reply without its `[zephyr] ` prefixes, cleaned like the transcript's text.
fn unprefixed(text: &str) -> String {
    let lines: Vec<String> = (tidy(text).lines())
        .map(|l| l.strip_prefix("[zephyr] ").unwrap_or(l).to_string())
        .collect();
    lines.join("\n")
}

/// `45s`, `3m`, `2h`, `23h 57m`, `3d 4h`.
fn span(secs: i64) -> String {
    let s = secs.max(0);
    match s {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => match (s / 3600, s % 3600 / 60) {
            (h, 0) => format!("{h}h"),
            (h, m) => format!("{h}h {m}m"),
        },
        s => match (s / 86_400, s % 86_400 / 3600) {
            (d, 0) => format!("{d}d"),
            (d, h) => format!("{d}d {h}h"),
        },
    }
}

/// `text` broken at spaces into rows of at most `width` columns; a word wider than a row is
/// cut into rows of its own, so nothing is lost at the panel's edge.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    let mut row = String::new();
    for word in text.split(' ') {
        let mut word = word.to_string();
        while visible_width(&word) > width {
            if !row.is_empty() {
                out.push(std::mem::take(&mut row));
            }
            let (mut head, mut w) = (String::new(), 0);
            for c in word.chars() {
                let cw = visible_width(c.encode_utf8(&mut [0; 4]));
                // At least one character a row, or a glyph wider than the row never moves on.
                if w + cw > width && !head.is_empty() {
                    break;
                }
                head.push(c);
                w += cw;
            }
            word = word[head.len()..].to_string();
            out.push(head);
        }
        if !row.is_empty() && visible_width(&row) + 1 + visible_width(&word) > width {
            out.push(std::mem::take(&mut row));
        }
        if !row.is_empty() {
            row.push(' ');
        }
        row.push_str(&word);
    }
    if !row.is_empty() || out.is_empty() {
        out.push(row);
    }
    out
}

/// `text` padded with spaces to `width` columns.
fn pad(text: &str, width: usize) -> String {
    let fill = width.saturating_sub(visible_width(text));
    format!("{text}{}", " ".repeat(fill))
}

fn cut(text: &str, width: usize) -> String {
    truncate_to_width(text, width, glyph("…"))
}

/// The panel's rows: its keys, an editor, the facts form, a proposal's card, or the sections and
/// their settings.
pub fn rows(
    o: &Overlay,
    snap: &Snapshot,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    now: i64,
    width: usize,
) -> Vec<Row> {
    let s = &o.settings;
    if s.help {
        return help_rows(p);
    }
    if let Some(e) = &s.editing {
        return editor_rows(o, e, snap, p, tz, now, width);
    }
    if let Some(form) = &s.fact_form {
        return fact_form_rows(s, form, p, width);
    }
    let card = (s.waiting.iter().flatten()).find(|pr| Some(pr.id) == s.card);
    if let Some(pr) = card {
        return card_rows(s, pr, p, tz, width);
    }
    main_rows(s, snap, p, tz, width)
}

/// The sections on the left, the open one's rows on the right, and what the chosen row is.
fn main_rows(
    s: &SettingsPanel,
    snap: &Snapshot,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    width: usize,
) -> Vec<Row> {
    let Some(sections) = &s.sections else {
        let listing = format!("listing settings{}", glyph("…"));
        return vec![Row::Line(p.muted.fg(&listing))];
    };
    let mut out = vec![];
    if let Some(problem) = &s.problem {
        out.push(Row::Line(p.error.fg(problem)));
    }
    let count = s.waiting.as_ref().map_or(snap.proposals as usize, Vec::len);
    let tabs = s.tabs();
    let names: Vec<String> = (tabs.iter())
        .map(|t| match t {
            Tab::Section(i) => sections[*i].name.clone(),
            Tab::Facts => "Private facts".into(),
            Tab::Waiting => format!("Waiting ({count})"),
        })
        .collect();
    let name_w = names.iter().map(|n| visible_width(n)).max().unwrap_or(0);
    // `› ` and a name, a column of its own, then a two-column gap.
    let left_w = name_w + 3;
    let right_w = width.saturating_sub(4).saturating_sub(left_w + 2);
    let right = match s.tab() {
        Tab::Waiting => waiting_cells(s, p, tz, right_w),
        Tab::Facts => fact_cells(s, p, right_w),
        Tab::Section(i) => setting_cells(&sections[i], s.row, p, right_w),
    };
    let focus = s.row.unwrap_or(s.at);
    for i in 0..names.len().max(right.len()) {
        let left = match names.get(i) {
            Some(name) => {
                let open = i == s.at;
                let here = open && s.row.is_none();
                // The open section keeps a muted mark while the keyboard is on its rows.
                let mark = match (open, here) {
                    (_, true) => p.accent.fg(glyph("›")),
                    (true, false) => p.muted.fg(glyph("›")),
                    _ => " ".into(),
                };
                let colour = match tabs.get(i) == Some(&Tab::Waiting) && count > 0 {
                    true => p.warning,
                    false => p.text,
                };
                let name = if open { bold(name) } else { name.clone() };
                let cell = pad(&format!("{mark} {}", colour.fg(&name)), left_w);
                if here {
                    p.surface.bg(&cell)
                } else {
                    cell
                }
            }
            None => " ".repeat(left_w),
        };
        let line = format!("{left}  {}", right.get(i).cloned().unwrap_or_default());
        out.push(match i == focus {
            true => Row::Focus(line),
            false => Row::Line(line),
        });
    }
    let about = match (s.entry(), s.proposal(), s.tab()) {
        (Some(e), ..) => about_setting(e, &s.strong),
        (_, Some(pr), _) => vec![about_proposal(pr, &s.strong, tz)],
        (.., Tab::Facts) => vec![
            "Kept out of outgoing calls: a call holding one waits for you, as approvals.facts \
             says. A value is typed here, masked, and never shown again."
                .into(),
        ],
        _ => vec![],
    };
    let inner = width.saturating_sub(4);
    if !about.is_empty() {
        out.push(Row::Line(String::new()));
        let lines = about.iter().flat_map(|l| wrap(l, inner));
        out.extend(lines.map(|l| Row::Line(p.muted.fg(&l))));
    }
    if let Some(name) = &s.fact_confirm {
        let ask = format!("delete the private fact {name}? y deletes it, any other key keeps it");
        out.extend(wrap(&ask, inner).iter().map(|l| Row::Line(p.warning.fg(l))));
    }
    out.extend(note_rows(s, p, inner));
    out
}

/// One `label  value · source` per setting; read-only entries muted, with their help.
fn setting_cells(section: &Section, row: Option<usize>, p: &Palette, w: usize) -> Vec<String> {
    if section.settings.is_empty() {
        return vec![p.muted.fg("  nothing to set here yet")];
    }
    let label_w = column(section.settings.iter().map(|e| e.label.as_str()));
    let value_w = (section.settings.iter())
        .filter(|e| !e.read_only())
        .map(|e| visible_width(&e.shown()))
        .max()
        .unwrap_or(0)
        .min(w / 2);
    (section.settings.iter().enumerate())
        .map(|(j, e)| {
            let sel = row == Some(j);
            let mark = if sel {
                p.accent.fg(glyph("›"))
            } else {
                " ".into()
            };
            let label = pad(&e.label, label_w);
            let label = if sel { bold(&label) } else { label };
            let body = if e.read_only() {
                let what = e.value.as_deref().unwrap_or(&e.help);
                p.muted.fg(&format!("{label}  {what}"))
            } else {
                let value = cut(&e.shown(), value_w);
                let source = match e.source.as_str() {
                    s if s.starts_with("agent") => p.warning.fg(s),
                    s if s.starts_with("until") => p.accent.fg(s),
                    s => p.muted.fg(s),
                };
                format!(
                    "{}  {} {} {source}",
                    p.text.fg(&label),
                    p.text.fg(&pad(&value, value_w)),
                    p.muted.fg(glyph("·"))
                )
            };
            let cell = cut(&format!("{mark} {body}"), w);
            match sel {
                true => p.surface.bg(&pad(&cell, w)),
                false => cell,
            }
        })
        .collect()
}

/// One `#id  card  3m ago` per proposal, the card cut to leave the age in view: for a held
/// action its class and summary, and for one waiting out its delay when it goes.
fn waiting_cells(s: &SettingsPanel, p: &Palette, tz: &jiff::tz::TimeZone, w: usize) -> Vec<String> {
    let list = match &s.waiting {
        None => return vec![p.muted.fg(&format!("  listing{}", glyph("…")))],
        Some(l) if l.is_empty() => return vec![p.muted.fg("  nothing waits for you")],
        Some(l) => l,
    };
    let id_w = list
        .iter()
        .map(|pr| pr.id.to_string().len() + 1)
        .max()
        .unwrap_or(0);
    (list.iter().enumerate())
        .map(|(j, pr)| {
            let sel = s.row == Some(j);
            let mark = if sel {
                p.accent.fg(glyph("›"))
            } else {
                " ".into()
            };
            let age = match (pr.delayed(), pr.due_at) {
                (true, Some(at)) => format!("at {}", crate::settings::clock(at, tz)),
                _ => format!("{} ago", span(pr.age_secs)),
            };
            let room = w.saturating_sub(2 + id_w + 2 + 2 + visible_width(&age));
            let text = match (pr.action(), &pr.summary) {
                (true, Some(summary)) => format!("{} {} {summary}", pr.what().1, glyph("·")),
                _ => one_line(&pr.card),
            };
            let card = cut(&text, room);
            let id = pad(&format!("#{}", pr.id), id_w);
            let id = if sel { bold(&id) } else { id };
            let cell = format!(
                "{mark} {}  {}  {}",
                p.accent.fg(&id),
                p.text.fg(&pad(&card, room)),
                p.muted.fg(&age)
            );
            let cell = cut(&cell, w);
            match sel {
                true => p.surface.bg(&pad(&cell, w)),
                false => cell,
            }
        })
        .collect()
}

/// One `name  kind · allow: targets` per private fact; never a value, which the daemon never
/// sends.
fn fact_cells(s: &SettingsPanel, p: &Palette, w: usize) -> Vec<String> {
    let mut out = vec![];
    if let Some(problem) = &s.facts_problem {
        out.push(cut(&p.error.fg(&format!("  {problem}")), w));
    }
    let list = match &s.facts {
        None => return vec![p.muted.fg(&format!("  listing{}", glyph("…")))],
        Some(l) if l.is_empty() => {
            out.push(p.muted.fg("  none yet: a adds one"));
            return out;
        }
        Some(l) => l,
    };
    let name_w = column(list.iter().map(|f| f.name.as_str()));
    let kind_w = list
        .iter()
        .map(|f| visible_width(&f.kind))
        .max()
        .unwrap_or(0);
    out.extend((list.iter().enumerate()).map(|(j, f)| {
        let sel = s.row == Some(j);
        let mark = if sel {
            p.accent.fg(glyph("›"))
        } else {
            " ".into()
        };
        let name = pad(&f.name, name_w);
        let name = if sel { bold(&name) } else { name };
        let allow = match f.allow.as_slice() {
            [] => "none".to_string(),
            a => a.join(", "),
        };
        let cell = format!(
            "{mark} {}  {} {} {}",
            p.text.fg(&name),
            p.text.fg(&pad(&f.kind, kind_w)),
            p.muted.fg(glyph("·")),
            p.muted.fg(&format!("allow: {allow}"))
        );
        let cell = cut(&cell, w);
        match sel {
            true => p.surface.bg(&pad(&cell, w)),
            false => cell,
        }
    }));
    out
}

/// The text a card shows, on one line: the agent shaped it, so nothing in it moves the cursor.
fn one_line(text: &str) -> String {
    tidy(text).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// What the chosen setting is, what it takes and who may change it.
fn about_setting(e: &Entry, strong: &[String]) -> Vec<String> {
    // The key first, since rows can share a label; a read-only row without a value already
    // shows its help.
    let dot = format!(" {} ", glyph("·"));
    let help = !e.help.is_empty() && !(e.read_only() && e.value.is_none());
    let mut out = vec![match help {
        true => format!("{}{dot}{}", e.key, e.help),
        false => e.key.clone(),
    }];
    if e.read_only() {
        out.push("read-only here: edit zephyr.toml to change it".into());
        return out;
    }
    let mut what = vec![takes(&e.kind, clearing(&e.key))];
    what.push(
        match e.access.as_str() {
            "tighten" => "the agent may tighten it",
            "propose" => "the agent asks you first",
            _ => "only you change it",
        }
        .into(),
    );
    out.push(what.join(&dot));
    if e.strong && !strong.is_empty() {
        out.push(format!("loosening it needs {}", strong.join(" or ")));
    }
    out
}

fn about_proposal(pr: &Proposal, strong: &[String], tz: &jiff::tz::TimeZone) -> String {
    let dot = format!(" {} ", glyph("·"));
    let mut what = vec![match pr.kind.as_str() {
        _ if pr.action() => format!("{} {}", pr.what().0, pr.what().1),
        "setting" => "a setting".to_string(),
        "command" => "a command".to_string(),
        other => other.to_string(),
    }];
    what.extend(pr.key.clone());
    what.push(when(pr, tz));
    if pr.strong {
        what.push(needs(strong));
    }
    what.join(&dot)
}

/// `goes at 14:05` for an action waiting out its delay, else `expires in 23h 57m`.
fn when(pr: &Proposal, tz: &jiff::tz::TimeZone) -> String {
    match (pr.delayed(), pr.due_at) {
        (true, Some(at)) => format!("goes at {}", crate::settings::clock(at, tz)),
        _ => format!("expires in {}", span(pr.expires_in_secs)),
    }
}

fn needs(strong: &[String]) -> String {
    match strong {
        [] => "needs a strong channel".into(),
        s => format!("needs a strong channel: {}", s.join(" or ")),
    }
}

/// The last answer, wrapped: green when it went through, red with the reason when not.
fn note_rows(s: &SettingsPanel, p: &Palette, width: usize) -> Vec<Row> {
    let Some((ok, text)) = &s.note else {
        return vec![];
    };
    let colour = if *ok { p.success } else { p.error };
    (text.lines().flat_map(|l| wrap(l, width)))
        .map(|l| Row::Line(colour.fg(&l)))
        .collect()
}

/// A proposal's whole card, wrapped: what `/ok` runs, how long it has waited and who may decide.
/// A held action's card is the gate's, as the chat got it.
fn card_rows(
    s: &SettingsPanel,
    pr: &Proposal,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    width: usize,
) -> Vec<Row> {
    let what = match (pr.kind.as_str(), &pr.key) {
        _ if pr.action() => format!("{} {}", pr.what().0, pr.what().1),
        ("setting", Some(key)) => format!("a setting: {key}"),
        (kind, _) => format!("a {kind}"),
    };
    let head = match (pr.action(), pr.delayed()) {
        (true, true) => format!("delayed #{}", pr.id),
        (true, false) => format!("held #{}", pr.id),
        _ => format!("proposal #{}", pr.id),
    };
    let mut out = vec![Row::Head(head, what)];
    let inner = width.saturating_sub(4).max(1);
    let card = match pr.action() {
        true => unprefixed(&pr.card),
        false => tidy(&pr.card),
    };
    for text in card.lines() {
        out.extend(wrap(text, inner).iter().map(|l| Row::Line(p.text.fg(l))));
    }
    out.push(Row::Line(String::new()));
    let dot = format!(" {} ", glyph("·"));
    // What it would change, as it stands now.
    let now = (s.sections.iter().flatten())
        .flat_map(|x| &x.settings)
        .find(|e| pr.key.as_deref() == Some(e.key.as_str()));
    if let Some(e) = now {
        let value = e.value.as_deref().unwrap_or("none");
        let line = format!("now {value}{dot}{}", e.source);
        out.extend(wrap(&line, inner).iter().map(|l| Row::Line(p.muted.fg(l))));
    }
    let mut times = vec![format!("waited {}", span(pr.age_secs)), when(pr, tz)];
    if pr.strong {
        times.push(needs(&s.strong));
    }
    let when = wrap(&times.join(&dot), inner);
    out.extend(when.iter().map(|l| Row::Line(p.muted.fg(l))));
    out.extend(note_rows(s, p, inner));
    out
}

/// The editor: what the setting is, then a list to pick from or a field to type in, then the
/// time limit once asked for, and the last refusal.
#[allow(clippy::too_many_arguments)]
fn editor_rows(
    o: &Overlay,
    e: &Editor,
    snap: &Snapshot,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    now: i64,
    width: usize,
) -> Vec<Row> {
    let mut out = vec![Row::Head(e.label.clone(), e.key.clone())];
    if !e.help.is_empty() {
        out.push(Row::Line(p.muted.fg(&e.help)));
    }
    out.push(Row::Line(p.muted.fg(&takes(&e.kind, clearing(&e.key)))));
    let field_line = |label: &str, text: &str, here: bool| {
        let (mark, cursor) = match here {
            true => (p.accent.fg(glyph("›")), CURSOR_MARKER),
            false => (" ".into(), ""),
        };
        let label = p.muted.fg(&format!("{label:<7}"));
        Row::Line(format!(" {mark} {label}{}{cursor}", p.text.fg(text)))
    };
    match field(&e.kind) {
        Field::List(values) => {
            out.extend(values.into_iter().map(|v| {
                let unset = v == "none";
                let v = if unset { clearing(&e.key).into() } else { v };
                Row::Item {
                    current: e.value.as_deref() == Some(v.as_str()) || (unset && e.value.is_none()),
                    pick: v.clone(),
                    label: v,
                    note: String::new(),
                }
            }));
        }
        Field::Model { optional } => {
            let prompt = p.accent.fg(glyph("›"));
            let cursor = if e.on_until { "" } else { CURSOR_MARKER };
            out.push(Row::Line(match o.query.is_empty() {
                true => format!("{prompt} {cursor}{}", p.muted.fg("type to filter")),
                false => format!("{prompt} {}{cursor}", p.text.fg(&o.query)),
            }));
            out.extend(model_rows(o, e, optional, snap, p, tz, now));
        }
        Field::Typed => out.push(field_line("value", &e.text, !e.on_until)),
    }
    if let Some(until) = &e.until {
        out.push(field_line("for", until, e.on_until));
        out.push(Row::Line(p.muted.fg(
            "  30m, 2h or 1d (30s to 30d), then it goes back; empty is for good",
        )));
    }
    if e.sent {
        out.push(Row::Line(p.muted.fg(&format!("saving{}", glyph("…")))));
    }
    out.extend(note_rows(&o.settings, p, width.saturating_sub(4)));
    out
}

/// The facts form: name, kind, the value masked, where it may go; then how to save it and the
/// last refusal.
fn fact_form_rows(s: &SettingsPanel, form: &FactForm, p: &Palette, width: usize) -> Vec<Row> {
    let title = match form.editing {
        true => format!("edit {}", form.fields[NAME].2),
        false => "add a private fact".into(),
    };
    let mut out = vec![Row::Head(title, String::new())];
    out.extend(field_rows(&form.fields, form.at, p));
    let inner = width.saturating_sub(4);
    let mut help = vec![
        "tab next field, enter on the last saves, esc back".to_string(),
        "kind: text, digits or email (space or ←→ turns it); may go to: targets it may reach \
         without a check, comma separated"
            .replace("←→", &format!("{}{}", glyph("←"), glyph("→"))),
    ];
    if form.editing {
        help.push("an empty value keeps the saved one".into());
    }
    help.push("the value is never shown again".into());
    for l in help.iter().flat_map(|l| wrap(l, inner)) {
        out.push(Row::Line(p.muted.fg(&l)));
    }
    out.extend(note_rows(s, p, inner));
    out
}

/// The model picker's groups, filtered by what is typed, each pick the model's id; `none`
/// first for a setting that may follow the account instead.
#[allow(clippy::too_many_arguments)]
fn model_rows(
    o: &Overlay,
    e: &Editor,
    optional: bool,
    snap: &Snapshot,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    now: i64,
) -> Vec<Row> {
    let words: Vec<String> = o.query.split_whitespace().map(str::to_lowercase).collect();
    let fits = |hay: String| {
        words
            .iter()
            .all(|w| hay.to_lowercase().contains(w.as_str()))
    };
    let mut out = vec![];
    if optional && fits("none default follow the account".into()) {
        out.push(Row::Head("none".into(), String::new()));
        out.push(Row::Item {
            pick: "none".into(),
            label: "none".into(),
            note: "follow the account's model".into(),
            current: e.value.is_none(),
        });
    }
    for (head, note, items) in model_groups(o, snap, tz, now) {
        let hit: Vec<Row> = (items.into_iter())
            .filter_map(|r| match r {
                Row::Item {
                    pick, label, note, ..
                } if fits(format!("{head} {label} {note} {pick}")) => {
                    let id = pick.strip_prefix("/model ").unwrap_or(&pick).to_string();
                    // The accounts' model lists come from the daemon too.
                    Some(Row::Item {
                        current: e.value.as_deref() == Some(id.as_str()),
                        pick: id,
                        label: line(&label),
                        note: line(&note),
                    })
                }
                _ => None,
            })
            .collect();
        if !hit.is_empty() {
            out.push(Row::Head(line(&head), line(&note)));
            out.extend(hit);
        }
    }
    if out.is_empty() {
        out.push(Row::Line(p.muted.fg("nothing matches")));
    }
    if o.models.is_none() {
        out.push(Row::Line(
            p.muted.fg(&format!("listing models{}", glyph("…"))),
        ));
    }
    out
}

/// `?`: the panel's keys.
fn help_rows(p: &Palette) -> Vec<Row> {
    let keys = [
        (glyph("↑↓"), "choose a section, a setting or a proposal"),
        (glyph("→"), "into the section; ← back to the sections"),
        (
            glyph("⏎"),
            "edit: a list, the model picker or a field; Waiting: the whole card",
        ),
        ("space", "turn a true-or-false setting over at once"),
        (
            "t",
            "change it for a while (30m, 2h, 1d), then it goes back",
        ),
        ("tab", "in an editor: the time limit"),
        ("o", "Waiting: approve it"),
        ("1", "Waiting: approve once, not remembering the recipients"),
        ("c", "Waiting: cancel a delayed action before it goes"),
        ("d", "Waiting: drop it; Private facts: delete one"),
        ("a", "Private facts: add one, its value typed masked"),
        (
            "e",
            "Private facts: edit one; an empty value keeps the saved one",
        ),
        ("esc", "back, or close the panel"),
    ];
    let mut out = vec![Row::Head("keys".into(), "esc goes back".into())];
    out.extend(keys.iter().map(|(k, what)| {
        Row::Line(format!(
            "  {}{}",
            p.text.fg(&format!("{k:<7}")),
            p.muted.fg(what)
        ))
    }));
    out
}

/// The panel's keys: see [`help_rows`].
pub fn key(o: &mut Overlay, k: KeyEvent, rows: &[Row]) -> Action {
    if o.settings.help {
        o.settings.help = false;
        return Action::None;
    }
    if o.settings.editing.is_some() {
        return editor_key(o, k, rows);
    }
    if o.settings.fact_form.is_some() {
        return fact_form_key(&mut o.settings, k);
    }
    if let Some(name) = o.settings.fact_confirm.take() {
        return match k.code {
            KeyCode::Char('y') => {
                o.settings.fact_sent = Some((false, name.clone()));
                Action::FactsRm(name)
            }
            _ => Action::None,
        };
    }
    if let Some(id) = o.settings.card {
        return match k.code {
            KeyCode::Esc | KeyCode::Left => {
                o.settings.card = None;
                Action::None
            }
            KeyCode::Char(c) => decide(o, id, c),
            _ => Action::None,
        };
    }
    let s = &mut o.settings;
    if s.sections.is_none() {
        return match k.code {
            KeyCode::Esc => Action::Close,
            _ => Action::None,
        };
    }
    let total = s.tabs().len();
    let len = s.len();
    let page = o.page.max(1);
    let last = |n: usize| n.saturating_sub(1);
    match (s.row, k.code) {
        (_, KeyCode::Esc) => return Action::Close,
        (_, KeyCode::Char('?')) => s.help = true,
        (None, code) => {
            let at = match code {
                KeyCode::Up => s.at.saturating_sub(1),
                KeyCode::Down => (s.at + 1).min(last(total)),
                KeyCode::PageUp => s.at.saturating_sub(page),
                KeyCode::PageDown => (s.at + page).min(last(total)),
                KeyCode::Home => 0,
                KeyCode::End => last(total),
                _ => s.at,
            };
            if at != s.at {
                s.at = at;
                s.note = None;
            } else if matches!(code, KeyCode::Enter | KeyCode::Right | KeyCode::Tab) && len > 0 {
                s.row = Some(0);
                s.note = None;
            } else if code == KeyCode::Char('a') && s.tab() == Tab::Facts {
                // Nothing to go into yet, so a fact is added from the section's own name too.
                s.note = None;
                s.fact_form = Some(FactForm::new());
            }
        }
        (Some(r), code) => {
            // Any key shows the owner the row a reload moved the cursor to; a decision on it
            // waits for the next one.
            if let Some(gone) = s.moved.take() {
                if let (Some(pr), KeyCode::Char('o' | '1' | 'c' | 'd')) = (s.proposal(), code) {
                    let id = pr.id;
                    let said = format!("#{gone} is no longer waiting; the cursor is on #{id}");
                    s.note = Some((false, format!("{said}: press again to decide it")));
                    return Action::None;
                }
            }
            let row = match code {
                KeyCode::Up => r.saturating_sub(1),
                KeyCode::Down => (r + 1).min(last(len)),
                KeyCode::PageUp => r.saturating_sub(page),
                KeyCode::PageDown => (r + page).min(last(len)),
                KeyCode::Home => 0,
                KeyCode::End => last(len),
                _ => r,
            };
            if row != r {
                s.row = Some(row);
                s.note = None;
                return Action::None;
            }
            if matches!(code, KeyCode::Left | KeyCode::BackTab) {
                s.row = None;
                s.note = None;
                return Action::None;
            }
            if s.tab() == Tab::Facts {
                fact_key(s, code);
                return Action::None;
            }
            if let Some(pr) = s.proposal() {
                let id = pr.id;
                return match code {
                    KeyCode::Enter => {
                        s.card = Some(id);
                        s.note = None;
                        Action::None
                    }
                    KeyCode::Char(c) => decide(o, id, c),
                    _ => Action::None,
                };
            }
            let Some(e) = s.entry().cloned() else {
                return Action::None;
            };
            match code {
                KeyCode::Enter => edit(o, &e, false),
                KeyCode::Char('t') => edit(o, &e, true),
                // A true-or-false setting turns over at once.
                KeyCode::Char(' ') if e.bool() && !e.read_only() => {
                    s.note = None;
                    let value = match e.value.as_deref() {
                        Some("true") => "false",
                        _ => "true",
                    };
                    return Action::SettingsSet {
                        key: e.key,
                        value: value.into(),
                        until: None,
                    };
                }
                _ => {}
            }
        }
    }
    Action::None
}

/// `o` approves, `1` approves once, `d` drops, `c` cancels a delayed action. Once differs from
/// ok only for an action whose class asks about new targets: elsewhere `1` approves as `o` does
/// (and is not offered), so the daemon is never asked for a once that means nothing.
fn decide(o: &mut Overlay, id: i64, c: char) -> Action {
    let Some(pr) = (o.settings.waiting.iter().flatten()).find(|p| p.id == id) else {
        return Action::None;
    };
    let verb = match c {
        'o' => "ok",
        '1' if pr.offers_once() => "once",
        '1' => "ok",
        'd' => "no",
        'c' if pr.delayed() => "cancel",
        _ => return Action::None,
    };
    o.settings.note = None;
    Action::Decide {
        id,
        verb: verb.into(),
    }
}

/// Opens the editor for `e`; `timed` with the keyboard on a time limit. Read-only entries, the
/// subagents and the rules say where they change instead.
fn edit(o: &mut Overlay, e: &Entry, timed: bool) {
    let s = &mut o.settings;
    // The facts' values are typed in their own section.
    if e.key == "facts" {
        if let Some(at) = s.tabs().iter().position(|t| *t == Tab::Facts) {
            (s.at, s.row, s.note) = (at, None, None);
            return;
        }
    }
    let elsewhere = match e.key.as_str() {
        _ if e.read_only() => Some(format!(
            "{} is read-only here: edit zephyr.toml to change it",
            e.label
        )),
        "subagent" => Some("change subagents with /subagent".into()),
        "rule" => Some("change the rules with /rule (/rules lists them)".into()),
        _ => None,
    };
    if let Some(why) = elsewhere {
        s.note = Some((false, why));
        return;
    }
    let typed = field(&e.kind) == Field::Typed;
    s.note = None;
    s.editing = Some(Editor {
        key: e.key.clone(),
        label: e.label.clone(),
        help: e.help.clone(),
        kind: e.kind.clone(),
        value: e.value.clone(),
        text: e.value.clone().filter(|_| typed).unwrap_or_default(),
        until: timed.then(String::new),
        on_until: timed,
        sent: false,
    });
    o.query.clear();
    o.top = 0;
    o.selected = 0;
    // The list starts on the value in effect; turning a true-or-false setting over for a while
    // starts on the other one.
    o.placed = false;
    if timed && e.bool() {
        o.placed = true;
        o.selected = usize::from(e.value.as_deref() == Some("true"));
    }
}

/// A key on the private facts' rows: `a` adds one, Enter or `e` edits the chosen one, `d` asks
/// to delete it.
fn fact_key(s: &mut SettingsPanel, code: KeyCode) {
    let chosen = s.fact().cloned();
    match (code, chosen) {
        (KeyCode::Char('a'), _) => s.fact_form = Some(FactForm::new()),
        (KeyCode::Enter | KeyCode::Char('e'), Some(f)) => s.fact_form = Some(FactForm::edit(&f)),
        (KeyCode::Char('d'), Some(f)) => s.fact_confirm = Some(f.name),
        _ => return,
    }
    s.note = None;
}

/// The facts form's keys. It takes every printable key while it is open; the kind turns with
/// Space or ←/→.
fn fact_form_key(s: &mut SettingsPanel, k: KeyEvent) -> Action {
    let form = s.fact_form.as_mut().expect("open");
    let last = form.fields.len() - 1;
    match k.code {
        KeyCode::Esc => {
            s.fact_form = None;
            s.note = None;
        }
        KeyCode::Tab | KeyCode::Down => form.at = (form.at + 1).min(last),
        KeyCode::BackTab | KeyCode::Up => form.at = form.at.saturating_sub(1),
        KeyCode::Enter if form.at < last => form.at += 1,
        KeyCode::Enter => return save_fact(s),
        KeyCode::Left if form.at == KIND => form.turn_kind(-1),
        KeyCode::Right | KeyCode::Char(' ') if form.at == KIND => form.turn_kind(1),
        KeyCode::Backspace => form.backspace(),
        KeyCode::Char(c) if !k.modifiers.contains(KeyModifiers::CONTROL) => {
            form.type_text(c.encode_utf8(&mut [0; 4]))
        }
        _ => {}
    }
    Action::None
}

/// Saves the form: checked here first (`Fact::check`, whose reasons never quote a value), so a
/// value too short is fixed in place rather than typed again; the daemon checks it again. A
/// name already saved is edited with `e`, never overwritten by an add.
fn save_fact(s: &mut SettingsPanel) -> Action {
    let form = s.fact_form.as_ref().expect("open");
    let fact = form.fact();
    let saved = (s.facts.iter().flatten()).any(|f| f.name == fact.name);
    let problem = match () {
        _ if !form.editing && saved => Some(format!("{} is saved; e edits it", fact.name)),
        // The saved value stays: the daemon checks the rest.
        _ if form.editing && fact.value.is_empty() => None,
        _ => fact.check().err().map(|e| format!("{e:#}")),
    };
    if let Some(why) = problem {
        s.note = Some((false, why));
        return Action::None;
    }
    s.fact_form = None;
    s.note = None;
    s.fact_sent = Some((true, fact.name.clone()));
    Action::FactsPut(Box::new(fact))
}

fn editor_key(o: &mut Overlay, k: KeyEvent, rows: &[Row]) -> Action {
    let picks = picks(rows);
    let last = picks.len().saturating_sub(1);
    let page = o.page.max(1);
    let typing = !k.modifiers.contains(KeyModifiers::CONTROL);
    let e = o.settings.editing.as_mut().expect("editing");
    let kind = field(&e.kind);
    let list = kind != Field::Typed;
    let model = matches!(kind, Field::Model { .. });
    match k.code {
        KeyCode::Esc => {
            o.settings.editing = None;
            o.settings.note = None;
            o.query.clear();
            o.top = 0;
        }
        KeyCode::Tab | KeyCode::BackTab => match e.until {
            None => {
                e.until = Some(String::new());
                e.on_until = true;
            }
            Some(_) => e.on_until = !e.on_until,
        },
        KeyCode::Enter if e.sent => {}
        KeyCode::Enter => {
            let value = match list {
                true => match picks.get(o.selected).map(|&i| &rows[i]) {
                    Some(Row::Item { pick, .. }) => pick.clone(),
                    _ => return Action::None,
                },
                false => e.text.trim().to_string(),
            };
            let until = (e.until.as_deref().map(str::trim))
                .filter(|u| !u.is_empty())
                .map(String::from);
            e.sent = true;
            let key = e.key.clone();
            o.settings.note = None;
            return Action::SettingsSet { key, value, until };
        }
        KeyCode::Up | KeyCode::Down if !list && e.until.is_some() => e.on_until = !e.on_until,
        // Moving in the list takes the keyboard back from the time limit.
        KeyCode::Up | KeyCode::Down | KeyCode::PageUp | KeyCode::PageDown if list => {
            e.on_until = false;
            o.selected = match k.code {
                KeyCode::Up => o.selected.saturating_sub(1),
                KeyCode::Down => (o.selected + 1).min(last),
                KeyCode::PageUp => o.selected.saturating_sub(page),
                _ => (o.selected + page).min(last),
            };
        }
        KeyCode::Backspace => match (e.on_until, model) {
            (true, _) => {
                e.until.as_mut().map(String::pop);
            }
            (false, true) => {
                o.query.pop();
                o.selected = 0;
            }
            (false, false) => {
                e.text.pop();
            }
        },
        KeyCode::Char(c) if typing => match (e.on_until, model, list) {
            (true, ..) => e.until.get_or_insert_with(String::new).push(c),
            (false, true, _) => {
                o.query.push(c);
                o.selected = 0;
            }
            (false, false, false) => e.text.push(c),
            // A list has nothing to type into: `t` asks for a time limit, as on the row.
            (false, false, true) if c == 't' => {
                e.until.get_or_insert_with(String::new);
                e.on_until = true;
            }
            _ => {}
        },
        _ => {}
    }
    Action::None
}

/// A paste goes where typing would, on one line.
pub fn paste(o: &mut Overlay, text: &str) {
    if let Some(form) = o.settings.fact_form.as_mut() {
        form.type_text(text);
        return;
    }
    let text = text.replace(['\r', '\n'], "");
    let Some(e) = o.settings.editing.as_mut() else {
        return;
    };
    match (e.on_until, field(&e.kind)) {
        (true, _) => e.until.get_or_insert_with(String::new).push_str(&text),
        (false, Field::Typed) => e.text.push_str(&text),
        (false, Field::Model { .. }) => {
            o.query.push_str(&text);
            o.selected = 0;
        }
        (false, Field::List(_)) => {}
    }
}

/// The footer's hints for where the keyboard is, most useful first; `esc` last.
pub fn hints(o: &Overlay) -> Vec<(&'static str, &'static str)> {
    let s = &o.settings;
    let mut out = match () {
        _ if s.help => vec![],
        _ if s.editing.is_some() => {
            let e = s.editing.as_ref().expect("editing");
            let mut out = vec![(glyph("⏎"), "save")];
            match field(&e.kind) {
                Field::Typed => {}
                Field::Model { .. } => {
                    out.push((glyph("↑↓"), "choose"));
                    out.push(("", "type to filter"));
                }
                Field::List(_) => out.push((glyph("↑↓"), "choose")),
            }
            out.push(("tab", "time limit"));
            out
        }
        _ if s.fact_form.is_some() => {
            let form = s.fact_form.as_ref().expect("open");
            let mut out = vec![("tab", "next field"), (glyph("⏎"), "save on the last")];
            if form.at == KIND {
                out.push(("space", "turn the kind"));
            }
            out
        }
        _ if s.fact_confirm.is_some() => vec![("y", "delete"), ("", "any other key keeps it")],
        _ if s.card.is_some() => {
            let card = (s.waiting.iter().flatten()).find(|pr| Some(pr.id) == s.card);
            card.map_or_else(Vec::new, decisions)
        }
        _ if s.row.is_none() && s.tab() == Tab::Facts => vec![
            (glyph("↑↓"), "choose"),
            (glyph("→"), "open"),
            ("a", "add"),
            ("?", "keys"),
        ],
        _ if s.row.is_none() => vec![(glyph("↑↓"), "choose"), (glyph("→"), "open"), ("?", "keys")],
        _ if s.tab() == Tab::Facts => vec![
            (glyph("⏎"), "edit"),
            ("a", "add"),
            ("d", "delete"),
            (glyph("↑↓"), "choose"),
            (glyph("←"), "sections"),
            ("?", "keys"),
        ],
        _ if s.waiting_tab() => {
            let mut out = vec![(glyph("⏎"), "card")];
            match s.proposal() {
                Some(pr) => out.extend(decisions(pr)),
                None => out.extend([("o", "ok"), ("1", "once"), ("d", "drop")]),
            }
            out.extend([(glyph("↑↓"), "choose"), (glyph("←"), "sections")]);
            out
        }
        _ => {
            let mut out = vec![];
            match s.entry() {
                Some(e) if e.read_only() => {}
                Some(e) => {
                    out.push((glyph("⏎"), "edit"));
                    if e.bool() {
                        out.push(("space", "toggle"));
                    }
                    out.push(("t", "time limit"));
                }
                None => {}
            }
            out.extend([
                (glyph("↑↓"), "choose"),
                (glyph("←"), "sections"),
                ("?", "keys"),
            ]);
            out
        }
    };
    let back = s.help
        || s.editing.is_some()
        || s.card.is_some()
        || s.fact_form.is_some()
        || s.fact_confirm.is_some();
    out.push(("esc", if back { "back" } else { "close" }));
    out
}

/// What may be decided about `pr`: `o`, then `1` where once differs (always for a setting), `c`
/// for a delayed action, `d`.
fn decisions(pr: &Proposal) -> Vec<(&'static str, &'static str)> {
    let mut out = vec![("o", "ok")];
    if pr.offers_once() {
        out.push(("1", "once"));
    }
    if pr.delayed() {
        out.push(("c", "cancel"));
    }
    out.push(("d", "drop"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_breaks_at_spaces_and_cuts_only_what_cannot_fit() {
        assert_eq!(wrap("a bb ccc", 4), ["a bb", "ccc"]);
        assert_eq!(wrap("abcdefg x", 3), ["abc", "def", "g x"]);
        assert_eq!(wrap("", 5), [""]);
        // A glyph wider than the row still moves on, one to a row.
        assert_eq!(wrap("日本", 1), ["日", "本"]);
    }

    #[test]
    fn each_kind_takes_its_own_editor_and_says_what_it_takes() {
        use serde_json::json;
        let float = json!({"type": "optional", "inner": {"type": "float", "min": 0.0, "max": 1e9}});
        assert_eq!(field(&float), Field::Typed);
        assert_eq!(takes(&float, "none"), "a number from 0, or none");
        let model = json!({"type": "optional", "inner": {"type": "model"}});
        assert_eq!(field(&model), Field::Model { optional: true });
        assert_eq!(
            field(&json!({"type": "bool"})),
            Field::List(vec!["true".into(), "false".into()])
        );
        let tokens = json!({"type": "int", "min": 1, "max": i64::MAX});
        assert_eq!(takes(&tokens, "none"), "a whole number from 1");
        assert_eq!(
            takes(&json!({"type": "int", "min": 1, "max": 32}), "none"),
            "a whole number, 1 to 32"
        );
        assert_eq!(
            takes(
                &json!({"type": "duration", "min_secs": 30, "max_secs": 60}),
                "none"
            ),
            "a time like 30m, 2h or 1d"
        );
        assert_eq!(span(86_220), "23h 57m");
        assert_eq!(span(3 * 86_400 + 4 * 3600), "3d 4h");
    }
}
