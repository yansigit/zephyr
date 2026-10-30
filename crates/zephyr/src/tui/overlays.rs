//! Panels over the bottom of the screen (the model picker, the status panel and help) and
//! slash autocomplete above the composer. Each draws from the shared [`State`]; mod.rs routes
//! the keys here while one is open.

use super::chrome::{short_model, spend, State};
use super::conn::{AccountModels, Snapshot};
use super::theme::{glyph, Palette};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use rpi_tui::{bold, truncate_to_width, visible_width, CURSOR_MARKER};

/// Every slash command zephyr answers: router.rs `command` and `control`, memory.rs
/// `command` and scheduler.rs `command`, plus the TUI's own `/help`. (group, usage, what it does)
pub const COMMANDS: &[(&str, &str, &str)] = &[
    (
        "Chat",
        "/new",
        "start a fresh conversation; memory and rules carry over",
    ),
    ("Chat", "/help", "this list"),
    ("Agent", "/stop", "stop the current reply"),
    ("Agent", "/model", "pick a model"),
    (
        "Agent",
        "/model <provider/model>",
        "switch the model after the current reply",
    ),
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
        "Gateway",
        "/status",
        "agent, accounts, channels, spend and jobs",
    ),
    ("Gateway", "/pause", "hold new messages until /resume"),
    ("Gateway", "/resume", "answer what waited"),
    ("Gateway", "/reload", "apply zephyr.toml and secrets.env"),
];

/// What goes in the composer for a command: up to its first argument.
pub fn insert(usage: &str) -> &str {
    usage.find('<').map_or(usage, |i| &usage[..i])
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    Model,
    Status,
    Help,
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

/// The panel's rows for `snap`, filtered by the query. `now` is in milliseconds.
pub fn rows(
    o: &Overlay,
    snap: &Snapshot,
    p: &Palette,
    tz: &jiff::tz::TimeZone,
    now: i64,
) -> Vec<Row> {
    let groups = match o.kind {
        Kind::Status => return status_rows(snap, p, tz, now),
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

/// The positions of the pickable rows.
fn picks(rows: &[Row]) -> Vec<usize> {
    (0..rows.len())
        .filter(|&i| matches!(rows[i], Row::Item { .. }))
        .collect()
}

/// Moves the selection, scrolls, filters, or picks.
pub fn key(o: &mut Overlay, k: KeyEvent, rows: &[Row]) -> Action {
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
    if o.kind != Kind::Status {
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
    o.selected = o.selected.min(picks.len().saturating_sub(1));
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
        rows(o, s, &DARK, &jiff::tz::TimeZone::UTC, NOW)
    }

    fn draw(o: &mut Overlay, s: &Snapshot, width: usize, height: usize) -> Vec<String> {
        let rows = list(o, s);
        plain(&render(o, &rows, &DARK, width, height))
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
        assert!(got[7].ends_with(" 1-5 of 25 ─╯"), "{got:?}");
        for _ in 0..6 {
            key(&mut o, press(KeyCode::Down), &rows);
        }
        let got = plain(&render(&mut o, &rows, &DARK, 80, 8));
        // The seventh command is /memory ok; the window follows it down.
        assert_eq!(
            got[6],
            "│  › /memory ok               let the agent see the waiting change             │"
        );
        assert!(got[7].ends_with(" 6-10 of 25 ─╯"), "{got:?}");
        key(&mut o, press(KeyCode::Down), &rows);
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick("/memory undo".into())
        );
        key(&mut o, press(KeyCode::End), &rows);
        assert_eq!(
            key(&mut o, press(KeyCode::Enter), &rows),
            Action::Pick("/reload".into())
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
        assert_eq!(heads, ["Chat", "Agent", "Memory", "Schedule", "Gateway"]);
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

    /// The memory and job commands in the list are the ones those modules answer.
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
                "Memory" => crate::memory::command(&dir, &dir, &text).is_some(),
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
        assert_eq!(usages("/").len(), COMMANDS.len());
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
}
