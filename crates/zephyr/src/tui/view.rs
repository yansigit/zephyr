//! The transcript: session history and live child events mapped onto rpi-tui components.
//! Assistant text uses upstream's `AssistantMessageComponent` and `Markdown`. User turns and
//! tool calls get compact components of their own: upstream's `UserMessageComponent` pads
//! with a blank row above and below, and its tool panel always shows a 12-line preview. The event folding follows upstream
//! `rpi-cli/src/transcript_view.rs` (pi-rust v0.12.0, MIT), cut down to what Zephyr's stream
//! carries: no bash panels, plans, todos or images.

use super::chrome::spread;
use super::theme::{glyph, Palette};
use rpi_tui::theme::Color;
use rpi_tui::{
    bold, truncate_to_width, visible_width, wrap_text_with_ansi, AssistantBlock,
    AssistantMessageComponent, AssistantMessageOptions, Component, Container,
};
use serde_json::Value;
use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// History shown on attach, counted in user turns.
const KEEP_TURNS: usize = 50;
/// Output rows of an expanded tool card.
/// shortcut: a long output is cut here, not paged; page it if people ask.
const EXPANDED_ROWS: usize = 30;

#[derive(Clone, Copy, PartialEq)]
enum Kind {
    None,
    User,
    Text,
    Card,
    Note,
}

pub struct View {
    /// What the scroll view renders.
    pub root: Arc<Container>,
    p: Palette,
    tz: jiff::tz::TimeZone,
    last: Kind,
    last_day: Option<jiff::civil::Date>,
    /// The assistant message being streamed.
    current: Option<Arc<AssistantMessageComponent>>,
    assistants: Vec<Arc<AssistantMessageComponent>>,
    cards: HashMap<String, Arc<ToolCard>>,
    /// The group the next tool card joins: the tools since the last text or turn.
    group: Option<Arc<ToolGroup>>,
    /// The timestamp of the assistant message that made each call, for history durations.
    called_at: HashMap<String, i64>,
    expanded: bool,
    hide_thinking: bool,
    /// The notice just shown, so the same one is not shown twice in a row.
    last_notice: Option<String>,
}

impl View {
    pub fn new(p: Palette, tz: jiff::tz::TimeZone) -> Self {
        View {
            root: Arc::new(Container::new()),
            p,
            tz,
            last: Kind::None,
            last_day: None,
            current: None,
            assistants: vec![],
            cards: HashMap::new(),
            group: None,
            called_at: HashMap::new(),
            expanded: false,
            hide_thinking: false,
            last_notice: None,
        }
    }

    /// Redraws from a `zephyr.subscribe` history: the last [`KEEP_TURNS`] user turns.
    pub fn replay(&mut self, messages: &[Value]) {
        self.root.clear();
        self.last = Kind::None;
        self.last_day = None;
        self.current = None;
        self.assistants.clear();
        self.cards.clear();
        self.group = None;
        self.called_at.clear();
        self.last_notice = None;
        let users: Vec<usize> = (0..messages.len())
            .filter(|&i| kind(&messages[i]) == "user")
            .collect();
        let start = match users.len().checked_sub(KEEP_TURNS) {
            Some(skip) if skip > 0 => users[skip],
            _ => 0,
        };
        if start > 0 {
            let line = self.p.muted.fg(&format!("{} earlier history", glyph("↑")));
            self.push(Kind::Note, Arc::new(Line(line)), None);
        }
        for m in &messages[start..] {
            match kind(m) {
                "user" => self.user(m),
                "assistant" => self.assistant(m, true),
                "toolResult" | "tool_result" => {
                    let id = m["toolCallId"].as_str().unwrap_or_default();
                    let took = m["timestamp"].as_i64().zip(self.called_at.get(id).copied());
                    let took = took.map(|(end, start)| {
                        Duration::from_millis(end.saturating_sub(start).max(0) as u64)
                    });
                    self.tool_end(id, m["toolName"].as_str(), m, took);
                }
                _ => {}
            }
        }
        self.settle();
    }

    /// Folds one live child event into the transcript.
    pub fn apply(&mut self, ev: &Value) {
        let m = &ev["message"];
        match ev["type"].as_str().unwrap_or_default() {
            "message_start" if kind(m) == "user" => self.user(m),
            "message_start" | "message_update" if kind(m) == "assistant" => {
                self.assistant(m, false)
            }
            "message_end" if kind(m) == "assistant" => self.assistant(m, true),
            "tool_execution_start" => {
                let id = ev["toolCallId"].as_str().unwrap_or_default();
                let name = ev["toolName"].as_str().unwrap_or_default();
                self.card(id, name, &ev["args"]).start();
            }
            "tool_execution_end" => {
                let id = ev["toolCallId"].as_str().unwrap_or_default();
                let mut result = ev["result"].clone();
                result["isError"] = ev["isError"].clone();
                self.tool_end(id, ev["toolName"].as_str(), &result, None);
            }
            "agent_settled" | "agent_end" => {
                self.settle();
                if let Some(e) = ev["error"].as_str().filter(|e| !e.is_empty()) {
                    self.error(e);
                }
            }
            _ => {}
        }
    }

    /// A dim one-line notice, e.g. `· reconnected`.
    pub fn notice(&mut self, text: &str) {
        // `/stop` both aborts the message (`stopped`) and replies `[zephyr] stopped`.
        if self.last_notice.as_deref() == Some(text) {
            return;
        }
        let line = self.p.muted.fg(&format!("{} {text}", glyph("·")));
        self.push(Kind::Note, Arc::new(Line(line)), None);
        self.last_notice = Some(text.to_string());
    }

    pub fn error(&mut self, text: &str) {
        let first = text.lines().next().unwrap_or_default();
        let line = self.p.error.fg(&format!("{} {first}", glyph("✗")));
        self.push(Kind::Note, Arc::new(Line(line)), None);
    }

    /// A `zephyr_reply`: a reply to the TUI that the agent's stream does not show. A one-line
    /// `[zephyr] …` (`stopped`, `paused`, `now on a/b`) is a notice; anything longer a card.
    pub fn reply(&mut self, command: Option<&str>, text: &str) {
        let text = clean(text);
        let text = text.trim_end();
        if let Some(line) = text.strip_prefix("[zephyr] ").filter(|t| !t.contains('\n')) {
            return self.notice(line);
        }
        let title = command.unwrap_or("zephyr").to_string();
        let card = SystemCard {
            title,
            text: text.to_string(),
            p: self.p,
        };
        self.push(Kind::Note, Arc::new(card), None);
    }

    pub fn toggle_expanded(&mut self) -> bool {
        self.expanded = !self.expanded;
        for card in self.cards.values() {
            card.state.lock().unwrap().expanded = self.expanded;
        }
        self.expanded
    }

    pub fn toggle_thinking(&mut self) -> bool {
        self.hide_thinking = !self.hide_thinking;
        for a in &self.assistants {
            a.set_hide_thinking(self.hide_thinking);
        }
        self.hide_thinking
    }

    /// Whether a tool card is running, so its timer needs frames.
    pub fn running(&self) -> bool {
        self.cards
            .values()
            .any(|c| c.state.lock().unwrap().started.is_some())
    }

    fn settle(&mut self) {
        if let Some(a) = self.current.take() {
            a.set_streaming(false);
        }
    }

    /// Adds an item with the transcript's one spacing rule: a blank row before each item,
    /// except a tool card right under its assistant text or another card.
    fn push(&mut self, kind: Kind, item: Arc<dyn Component>, mark: Option<String>) {
        self.last_notice = None;
        let tight = kind == Kind::Card && matches!(self.last, Kind::Text | Kind::Card);
        if self.last != Kind::None && !tight {
            self.root.add_child(Arc::new(Line(String::new())));
        }
        self.root.add_child(Arc::new(Gutter { mark, item }));
        self.last = kind;
    }

    fn user(&mut self, m: &Value) {
        let text = m["text"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| crate::rpc::message_text(m));
        let channel = m["origin"]["channel"].as_str().unwrap_or("tui");
        let who = if channel == crate::attach::CHANNEL {
            "you"
        } else {
            channel
        };
        let when = m["timestamp"]
            .as_i64()
            .filter(|&t| t > 0)
            .and_then(|t| jiff::Timestamp::from_millisecond(t).ok())
            .map(|t| t.to_zoned(self.tz.clone()));
        if let Some(when) = &when {
            if self.last_day != Some(when.date()) {
                self.last_day = Some(when.date());
                let label = when.strftime("%a %-d %b %Y").to_string();
                self.push(Kind::Note, Arc::new(Rule { label, p: self.p }), None);
            }
        }
        let turn = Bubble {
            who: who.to_string(),
            when: when.map(|t| t.strftime("%H:%M").to_string()),
            color: self.p.channel(channel),
            text,
            p: self.p,
        };
        self.push(Kind::User, Arc::new(turn), None);
    }

    fn assistant(&mut self, m: &Value, done: bool) {
        let blocks = blocks(m);
        let visible = blocks.iter().any(|b| match b {
            AssistantBlock::Text(t) | AssistantBlock::Thinking(t) => !t.trim().is_empty(),
            AssistantBlock::Image(_) => false,
        });
        if self.current.is_none() && visible {
            let a = Arc::new(AssistantMessageComponent::new(AssistantMessageOptions {
                output_pad: 0,
                ..Default::default()
            }));
            a.set_hide_thinking(self.hide_thinking);
            a.set_streaming(!done);
            self.assistants.push(a.clone());
            self.current = Some(a.clone());
            let mark = self.p.accent.fg(&bold(glyph("≋")));
            self.push(Kind::Text, a, Some(mark));
        }
        if let Some(a) = &self.current {
            a.update_blocks(&blocks);
        }
        for call in m["content"].as_array().into_iter().flatten() {
            if call["type"] != "toolCall" {
                continue;
            }
            let id = call["id"].as_str().unwrap_or_default();
            let name = call["name"].as_str().unwrap_or_default();
            if id.is_empty() || name.is_empty() {
                continue;
            }
            self.card(id, name, &call["arguments"]);
            if let Some(t) = m["timestamp"].as_i64() {
                self.called_at.insert(id.to_string(), t);
            }
        }
        if done {
            self.settle();
            match m["stopReason"].as_str() {
                Some("aborted") => self.notice("stopped"),
                Some("error") => {
                    let e = m["errorMessage"]
                        .as_str()
                        .unwrap_or("the model call failed");
                    self.error(e);
                }
                _ => {}
            }
        }
    }

    /// The card for a tool call, made on first sight.
    fn card(&mut self, id: &str, name: &str, args: &Value) -> Arc<ToolCard> {
        if let Some(card) = self.cards.get(id) {
            if !args.is_null() {
                card.state.lock().unwrap().summary = summary(name, args);
            }
            return card.clone();
        }
        let card = Arc::new(ToolCard {
            p: self.p,
            state: Mutex::new(CardState {
                name: name.to_string(),
                summary: summary(name, args),
                expanded: self.expanded,
                ..Default::default()
            }),
        });
        self.cards.insert(id.to_string(), card.clone());
        match self.group.as_ref().filter(|_| self.last == Kind::Card) {
            Some(group) => group.cards.lock().unwrap().push(card.clone()),
            None => {
                let group = Arc::new(ToolGroup {
                    p: self.p,
                    cards: Mutex::new(vec![card.clone()]),
                });
                self.group = Some(group.clone());
                self.push(Kind::Card, group, None);
            }
        }
        card
    }

    fn tool_end(&mut self, id: &str, name: Option<&str>, result: &Value, took: Option<Duration>) {
        if id.is_empty() {
            return;
        }
        let card = self.card(id, name.unwrap_or("tool"), &Value::Null);
        let mut s = card.state.lock().unwrap();
        let took = took.or_else(|| s.started.map(|t| t.elapsed()));
        s.started = None;
        s.took = took;
        s.done = Some(result["isError"] != true);
        s.output = clean(&crate::rpc::message_text(result));
    }
}

/// Text from tools and jobs, safe to draw: no escape sequences, `\r` or tabs, which would
/// shift the layout or reach the terminal. As upstream `tool_execution::normalized_output_lines`.
fn clean(text: &str) -> String {
    let text = rpi_tui::strip_ansi(text)
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let text = text.replace('\t', "    ");
    text.chars()
        .filter(|&c| c == '\n' || !c.is_control())
        .collect()
}

fn kind(m: &Value) -> &str {
    m["kind"]
        .as_str()
        .or(m["role"].as_str())
        .unwrap_or_default()
}

fn blocks(m: &Value) -> Vec<AssistantBlock> {
    m["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|b| match b["type"].as_str()? {
            "text" => Some(AssistantBlock::Text(b["text"].as_str()?.to_string())),
            "thinking" => Some(AssistantBlock::Thinking(
                b["thinking"].as_str()?.to_string(),
            )),
            _ => None,
        })
        .collect()
}

/// A tool call's arguments in a few words: the command, path or pattern when there is one.
fn summary(name: &str, args: &Value) -> String {
    let field = |k: &str| args[k].as_str().filter(|s| !s.is_empty());
    let text = match (name, field("pattern"), field("path")) {
        ("grep" | "find", Some(p), Some(dir)) => format!("{p} {} {dir}", glyph("·")),
        _ => ["command", "path", "pattern", "query", "url", "prompt"]
            .iter()
            .find_map(|k| field(k))
            .map(str::to_string)
            .unwrap_or_else(|| match args {
                Value::Null => String::new(),
                Value::Object(m) if m.is_empty() => String::new(),
                other => other.to_string(),
            }),
    };
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn elapsed(d: Duration) -> String {
    let s = d.as_secs_f64();
    if s < 10.0 {
        format!("{s:.1}s")
    } else if s < 60.0 {
        format!("{}s", s as u64)
    } else {
        format!("{}m{:02}s", s as u64 / 60, s as u64 % 60)
    }
}

/// A transcript item indented three columns, with an optional one-column mark in the gutter.
struct Gutter {
    mark: Option<String>,
    item: Arc<dyn Component>,
}

impl Component for Gutter {
    fn render(&self, width: usize) -> Vec<String> {
        let mut lines = self.item.render(width.saturating_sub(4).max(1));
        for (i, line) in lines.iter_mut().enumerate() {
            let lead = match (&self.mark, i) {
                (Some(mark), 0) => format!(" {mark} "),
                _ => "   ".to_string(),
            };
            line.insert_str(0, &lead);
        }
        lines
    }
    fn invalidate(&self) {
        self.item.invalidate();
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Below this width a bubble takes the whole row.
const BUBBLE_FULL_BELOW: usize = 86;

/// A user turn: a rounded bubble on the right, its channel and time in the top edge, in the
/// channel's colour; full width on narrow screens.
struct Bubble {
    who: String,
    when: Option<String>,
    color: Color,
    text: String,
    p: Palette,
}

impl Component for Bubble {
    fn render(&self, width: usize) -> Vec<String> {
        let (p, c) = (&self.p, self.color);
        let label = match &self.when {
            Some(t) => format!(
                "{} {} {}",
                c.fg(&bold(&self.who)),
                p.muted.fg(glyph("·")),
                p.muted.fg(t)
            ),
            None => c.fg(&bold(&self.who)),
        };
        let label_w = visible_width(&label);
        let full = width < BUBBLE_FULL_BELOW;
        let max = if full {
            width
        } else {
            (width * 7 / 10).max(label_w + 6)
        }
        .min(width);
        // "│ " text " │", so four columns of frame around the text.
        let lines = wrap_words(&self.text, max.saturating_sub(4).max(1));
        let inner = if full {
            width.saturating_sub(4)
        } else {
            lines
                .iter()
                .map(|l| visible_width(l))
                .max()
                .unwrap_or(0)
                .max(label_w + 2)
        };
        let outer = inner + 4;
        let indent = " ".repeat(width.saturating_sub(outer));
        let (h, v) = (glyph("─"), glyph("│"));
        let top_fill = h.repeat(outer.saturating_sub(label_w + 5));
        let mut rows = vec![format!(
            "{indent}{}{label}{}",
            c.fg(&format!("{}{h} ", glyph("╭"))),
            c.fg(&format!(" {top_fill}{}", glyph("╮")))
        )];
        for line in &lines {
            let pad = " ".repeat(inner.saturating_sub(visible_width(line)));
            let body = p.surface.bg(&format!(" {}{pad} ", p.text.fg(line)));
            rows.push(format!("{indent}{}{body}{}", c.fg(v), c.fg(v)));
        }
        rows.push(format!(
            "{indent}{}",
            c.fg(&format!(
                "{}{}{}",
                glyph("╰"),
                h.repeat(outer - 2),
                glyph("╯")
            ))
        ));
        rows
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Wraps plain text at word boundaries; a word longer than `width` is split. A line that fits
/// is kept as it is, spacing included, so aligned columns (`/status`) stay aligned.
fn wrap_words(text: &str, width: usize) -> Vec<String> {
    let width = width.max(1);
    let mut out = vec![];
    for line in text.lines() {
        let mut rest = line.trim_end();
        if visible_width(rest) <= width {
            out.push(rest.to_string());
            continue;
        }
        while visible_width(rest) > width {
            // The widest prefix that fits, then back to its last space.
            let mut fit = 0;
            for (i, ch) in rest.char_indices() {
                if visible_width(&rest[..i + ch.len_utf8()]) > width {
                    break;
                }
                fit = i + ch.len_utf8();
            }
            let cut = rest[..fit]
                .rfind(' ')
                .filter(|&i| i > 0)
                .unwrap_or(fit.max(1));
            out.push(rest[..cut].trim_end().to_string());
            rest = rest[cut..].trim_start();
        }
        if !rest.is_empty() {
            out.push(rest.to_string());
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// One pre-styled row.
struct Line(String);

impl Component for Line {
    fn render(&self, width: usize) -> Vec<String> {
        vec![truncate_to_width(&self.0, width, "…")]
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `──── Thu 30 Oct 2026 ────`, before the first turn of each day.
struct Rule {
    label: String,
    p: Palette,
}

impl Component for Rule {
    fn render(&self, width: usize) -> Vec<String> {
        let label = format!(" {} ", self.label);
        let side = width.saturating_sub(visible_width(&label)) / 2;
        let dash = glyph("─").repeat(side);
        vec![format!(
            "{}{}{}",
            self.p.dim.fg(&dash),
            self.p.muted.fg(&label),
            self.p.dim.fg(&dash)
        )]
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[derive(Default)]
struct CardState {
    name: String,
    summary: String,
    expanded: bool,
    /// Set while the tool runs (live only).
    started: Option<Instant>,
    took: Option<Duration>,
    /// `Some(true)` succeeded, `Some(false)` failed, `None` not finished.
    done: Option<bool>,
    output: String,
}

/// `▸ name  summary ……… ✓ 0.2s`; expanded, the output below. A failure always shows its first line.
struct ToolCard {
    p: Palette,
    state: Mutex<CardState>,
}

impl ToolCard {
    fn start(&self) {
        let mut s = self.state.lock().unwrap();
        if s.done.is_none() {
            s.started = Some(Instant::now());
        }
    }
}

impl Component for ToolCard {
    fn render(&self, width: usize) -> Vec<String> {
        let s = self.state.lock().unwrap();
        let p = &self.p;
        let chevron = glyph(if s.expanded { "▾" } else { "▸" });
        let time = s.took.or(s.started.map(|t| t.elapsed())).map(elapsed);
        let time = time.unwrap_or_default();
        let right = match s.done {
            Some(true) => format!("{} {}", p.success.fg(glyph("✓")), p.muted.fg(&time)),
            Some(false) => format!("{} {}", p.error.fg(glyph("✗")), p.muted.fg(&time)),
            None => format!("{} {}", p.accent.fg(glyph("●")), p.muted.fg(&time)),
        };
        let left = format!(
            "{} {}  {}",
            p.muted.fg(chevron),
            p.text.fg(&bold(&s.name)),
            p.muted.fg(&s.summary)
        );
        let mut lines = vec![spread(&left, right.trim_end(), width)];
        let output: Vec<&str> = s.output.lines().filter(|l| !l.trim().is_empty()).collect();
        let body = width.saturating_sub(2).max(1);
        if s.expanded {
            let rows: Vec<String> = output
                .iter()
                .flat_map(|l| wrap_text_with_ansi(l, body))
                .collect();
            for row in rows.iter().take(EXPANDED_ROWS) {
                let color = if s.done == Some(false) {
                    p.error
                } else {
                    p.muted
                };
                lines.push(format!("  {}", color.fg(row)));
            }
            if rows.len() > EXPANDED_ROWS {
                let more = format!("{} {} more lines", glyph("…"), rows.len() - EXPANDED_ROWS);
                lines.push(format!("  {}", p.muted.fg(&more)));
            }
        } else if s.done == Some(false) {
            if let Some(first) = output.first() {
                lines.push(format!(
                    "  {}",
                    p.error.fg(&truncate_to_width(first, body, "…"))
                ));
            }
        }
        lines
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// The tool calls between two texts. One call is its card; more fold into one line
/// (`▸ 3 tools  bash, read ×2 ……… ✓ 4.1s`) with any failure under it, and `^O` opens them as a
/// tree of cards.
struct ToolGroup {
    p: Palette,
    cards: Mutex<Vec<Arc<ToolCard>>>,
}

impl Component for ToolGroup {
    fn render(&self, width: usize) -> Vec<String> {
        let cards = self.cards.lock().unwrap();
        if cards.len() == 1 {
            return cards[0].render(width);
        }
        let p = &self.p;
        let states: Vec<_> = cards.iter().map(|c| c.state.lock().unwrap()).collect();
        let expanded = states.iter().any(|s| s.expanded);
        let mut names: Vec<(&str, usize)> = vec![];
        for s in &states {
            match names.iter_mut().find(|(n, _)| *n == s.name) {
                Some((_, k)) => *k += 1,
                None => names.push((&s.name, 1)),
            }
        }
        let names = names
            .iter()
            .map(|(n, k)| {
                if *k > 1 {
                    format!("{n} ×{k}")
                } else {
                    n.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        let running = states
            .iter()
            .find(|s| s.done.is_none() && s.started.is_some());
        let took: Duration = states
            .iter()
            .filter_map(|s| s.took.or(s.started.map(|t| t.elapsed())))
            .sum();
        let mark = if states.iter().any(|s| s.done.is_none()) {
            p.accent.fg(glyph("●"))
        } else if states.iter().any(|s| s.done == Some(false)) {
            p.error.fg(glyph("✗"))
        } else {
            p.success.fg(glyph("✓"))
        };
        let what = match running {
            Some(r) => format!("running {}", r.name),
            None => names,
        };
        let left = format!(
            "{} {}  {}",
            p.muted.fg(glyph(if expanded { "▾" } else { "▸" })),
            p.text.fg(&bold(&format!("{} tools", states.len()))),
            p.muted.fg(&what)
        );
        let mut lines = vec![spread(
            &left,
            &format!("{mark} {}", p.muted.fg(&elapsed(took))),
            width,
        )];
        let failed: Vec<usize> = (0..states.len())
            .filter(|&i| states[i].done == Some(false))
            .collect();
        drop(states);
        let shown: Vec<usize> = if expanded {
            (0..cards.len()).collect()
        } else {
            failed
        };
        for (n, &i) in shown.iter().enumerate() {
            let last = n + 1 == shown.len();
            let (first, more) = if last {
                (glyph("╰"), " ")
            } else {
                (glyph("├"), glyph("│"))
            };
            for (j, row) in cards[i]
                .render(width.saturating_sub(2).max(1))
                .into_iter()
                .enumerate()
            {
                let lead = if j == 0 { first } else { more };
                lines.push(format!("{} {row}", p.dim.fg(lead)));
            }
        }
        lines
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// A reply from zephyr itself (`/status`, `/model`, job output), boxed and titled.
struct SystemCard {
    title: String,
    text: String,
    p: Palette,
}

impl Component for SystemCard {
    fn render(&self, width: usize) -> Vec<String> {
        let p = &self.p;
        let inner = width.saturating_sub(4).max(1);
        let h = glyph("─");
        let (tl, tr, bl, br, v) = if glyph("─") == "─" {
            ("╭", "╮", "╰", "╯", "│")
        } else {
            ("+", "+", "+", "+", "|")
        };
        let title = format!(" {} ", self.title);
        let fill = width.saturating_sub(visible_width(&title) + 3);
        let mut lines = vec![format!(
            "{}{}{}",
            p.dim.fg(&format!("{tl}{h}")),
            p.muted.fg(&bold(&title)),
            p.dim.fg(&format!("{}{tr}", h.repeat(fill)))
        )];
        // Replies are plain lines (`/status` output), not markdown.
        let body = self.text.lines().flat_map(|l| wrap_words(l, inner));
        for row in body {
            let row = p.text.fg(&row);
            let pad = inner.saturating_sub(visible_width(&row));
            lines.push(format!(
                "{} {}{} {}",
                p.dim.fg(v),
                row,
                " ".repeat(pad),
                p.dim.fg(v)
            ));
        }
        let bottom = format!("{bl}{}{br}", h.repeat(width.saturating_sub(2)));
        lines.push(p.dim.fg(&bottom));
        lines
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn text(view: &View, width: usize) -> String {
        let lines = view.root.render(width);
        lines
            .iter()
            .map(|l| rpi_tui::strip_ansi(l).trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn view() -> View {
        View::new(super::super::theme::DARK, jiff::tz::TimeZone::UTC)
    }

    /// 2026-10-30 14:02 UTC.
    const T: i64 = 1_793_368_920_000;

    fn history() -> Vec<Value> {
        vec![
            json!({"kind": "user", "role": "user", "content": "[discord #7] remind me to renew the domain on friday",
                   "timestamp": T, "origin": {"channel": "discord", "id": 7}, "text": "remind me to renew the domain on friday"}),
            json!({"kind": "assistant", "role": "assistant", "timestamp": T + 1000, "content": [
                {"type": "text", "text": "Done. I scheduled it for Fri 09:00."},
                {"type": "toolCall", "id": "c1", "name": "schedule", "arguments": {"prompt": "renew domain", "when": "fri 09:00"}}]}),
            json!({"kind": "tool_result", "role": "toolResult", "toolCallId": "c1", "toolName": "schedule",
                   "content": [{"type": "text", "text": "job 3 added"}], "isError": false, "timestamp": T + 1200}),
            json!({"kind": "user", "role": "user", "content": "[tui #8] what's in deploy?", "timestamp": T + 180_000,
                   "origin": {"channel": "tui", "id": 8}, "text": "what's in deploy?"}),
            json!({"kind": "assistant", "role": "assistant", "timestamp": T + 181_000, "content": [
                {"type": "toolCall", "id": "c2", "name": "bash", "arguments": {"command": "ls deploy"}},
                {"type": "toolCall", "id": "c3", "name": "read", "arguments": {"path": "deploy/missing"}}]}),
            json!({"kind": "tool_result", "role": "toolResult", "toolCallId": "c2", "toolName": "bash",
                   "content": [{"type": "text", "text": "Dockerfile\nrun.sh"}], "isError": false, "timestamp": T + 181_100}),
            json!({"kind": "tool_result", "role": "toolResult", "toolCallId": "c3", "toolName": "read",
                   "content": [{"type": "text", "text": "ENOENT: no such file\nmore detail"}], "isError": true, "timestamp": T + 181_300}),
            json!({"kind": "assistant", "role": "assistant", "timestamp": T + 182_000,
                   "content": [{"type": "text", "text": "Two files."}]}),
            json!({"kind": "user", "role": "user", "content": "[cron #9] daily brief", "timestamp": T + 200_000,
                   "origin": {"channel": "cron", "id": 9}, "text": "daily brief"}),
        ]
    }

    #[test]
    fn history_maps_to_bubbles_tool_groups_and_gutter_marks() {
        let mut v = view();
        v.replay(&history());
        // Below 90 columns a bubble takes the whole row.
        let got = text(&v, 80);
        let want = "   ───────────────────────────── Fri 30 Oct 2026 ─────────────────────────────

   ╭─ discord · 14:02 ────────────────────────────────────────────────────────╮
   │ remind me to renew the domain on friday                                  │
   ╰──────────────────────────────────────────────────────────────────────────╯

 ≋ Done. I scheduled it for Fri 09:00.
   ▸ schedule  renew domain                                              ✓ 0.2s

   ╭─ you · 14:05 ────────────────────────────────────────────────────────────╮
   │ what's in deploy?                                                        │
   ╰──────────────────────────────────────────────────────────────────────────╯

   ▸ 2 tools  bash, read                                                 ✗ 0.4s
   ╰ ▸ read  deploy/missing                                              ✗ 0.3s
       ENOENT: no such file

 ≋ Two files.

   ╭─ cron · 14:05 ───────────────────────────────────────────────────────────╮
   │ daily brief                                                              │
   ╰──────────────────────────────────────────────────────────────────────────╯";
        assert_eq!(got, want, "\n{got}");
        assert!(!got.contains("[discord #7]") && !got.contains("[tui #"));
    }

    #[test]
    fn ctrl_o_expands_tool_output() {
        let mut v = view();
        v.replay(&history());
        assert!(v.toggle_expanded());
        let got = text(&v, 80);
        assert!(got.contains("   ▾ 2 tools  bash, read"), "{got}");
        assert!(got.contains("   ├ ▾ bash  ls deploy"), "{got}");
        assert!(
            got.contains("   │   Dockerfile\n   │   run.sh\n   ╰ ▾ read  deploy/missing"),
            "{got}"
        );
        assert!(got.contains("\n       more detail"), "{got}");
    }

    #[test]
    fn live_events_stream_into_the_same_shapes() {
        let mut v = view();
        v.replay(&[]);
        let user = json!({"kind": "user", "content": "[imessage #4] ping", "text": "ping",
                          "origin": {"channel": "imessage", "id": 4}});
        v.apply(&json!({"type": "message_start", "message": user}));
        let partial =
            json!({"kind": "assistant", "content": [{"type": "text", "text": "Checking"}]});
        v.apply(&json!({"type": "message_update", "message": partial}));
        let full = json!({"kind": "assistant", "content": [{"type": "text", "text": "Checking the logs."},
            {"type": "toolCall", "id": "t1", "name": "bash", "arguments": {"command": "tail log"}}]});
        v.apply(&json!({"type": "message_end", "message": full}));
        v.apply(&json!({"type": "tool_execution_start", "toolCallId": "t1", "toolName": "bash", "args": {"command": "tail log"}}));
        assert!(v.running());
        v.apply(&json!({"type": "tool_execution_end", "toolCallId": "t1", "toolName": "bash", "isError": false,
                        "result": {"content": [{"type": "text", "text": "ok"}]}}));
        assert!(!v.running());
        v.apply(&json!({"type": "agent_settled", "error": "rate limited; resets later"}));
        let got = text(&v, 60);
        assert!(got.starts_with("   ╭─ imessage ─"), "{got}");
        assert!(got.contains("   │ ping "), "{got}");
        assert_eq!(got.matches("Checking").count(), 1, "{got}");
        assert!(
            got.contains(" ≋ Checking the logs.\n   ▸ bash  tail log"),
            "{got}"
        );
        assert!(got.contains("✓ 0.0s"), "{got}");
        assert!(got.ends_with("   ✗ rate limited; resets later"), "{got}");
    }

    #[test]
    fn long_history_keeps_the_last_turns() {
        let mut msgs = vec![];
        for i in 0..(KEEP_TURNS + 3) {
            msgs.push(json!({"kind": "user", "content": format!("q{i}"), "text": format!("q{i}"), "origin": null}));
        }
        let mut v = view();
        v.replay(&msgs);
        let got = text(&v, 40);
        assert!(got.starts_with("   ↑ earlier history"), "{got}");
        assert!(!got.contains(" q2 ") && got.contains(" q3 "), "{got}");
    }

    #[test]
    fn wide_screens_put_bubbles_on_the_right_and_wrap_them_at_words() {
        let mut v = view();
        let long =
            "please check whether the nightly backup finished and tell me which files changed";
        v.replay(&[json!({"kind": "user", "text": long, "origin": {"channel": "tui", "id": 1}})]);
        let rows: Vec<String> = v
            .root
            .render(120)
            .iter()
            .map(|l| rpi_tui::strip_ansi(l))
            .collect();
        // At most 70% of the row, flush right, one column from the edge (the gutter's).
        let top = &rows[0];
        assert!(
            top.starts_with(&" ".repeat(30)) && top.trim_end().ends_with('╮'),
            "{top:?}"
        );
        assert_eq!(rpi_tui::visible_width(top.trim_end()), 119);
        let body: Vec<&str> = rows[1..rows.len() - 1]
            .iter()
            .map(|r| r.trim().trim_matches('│').trim())
            .collect();
        assert_eq!(
            body,
            [
                "please check whether the nightly backup finished and tell me which files",
                "changed"
            ],
            "{rows:#?}"
        );
    }

    #[test]
    fn words_wrap_whole_and_fitting_lines_keep_their_spacing() {
        assert_eq!(
            wrap_words("aaa bbb ccc ddd approves it", 14),
            ["aaa bbb ccc", "ddd approves", "it"]
        );
        assert_eq!(
            wrap_words("agent:   work   on a/b", 40),
            ["agent:   work   on a/b"]
        );
        assert_eq!(wrap_words("abcdefghij", 4), ["abcd", "efgh", "ij"]);
        assert_eq!(wrap_words("", 10), [""]);
        assert_eq!(
            wrap_words("한국어 단어 줄바꿈", 7),
            ["한국어", "단어", "줄바꿈"]
        );
    }

    #[test]
    fn one_line_zephyr_replies_are_notices() {
        let mut v = view();
        v.reply(Some("/stop"), "[zephyr] stopped");
        v.notice("stopped");
        v.reply(Some("/model a/b"), "[zephyr] now on a/b\n");
        v.reply(
            Some("/pause"),
            "[zephyr] paused: messages wait in the inbox until /resume",
        );
        assert_eq!(
            text(&v, 70),
            "   · stopped\n\n   · now on a/b\n\n   · paused: messages wait in the inbox until /resume"
        );
    }

    #[test]
    fn tool_output_and_replies_are_cleaned_of_control_characters() {
        let dirty = "a\tb\rc\x1b[31mred\x1b[0m\x1b]52;c;eA==\x07\x08end";
        let mut v = view();
        v.replay(&[
            json!({"kind": "assistant", "role": "assistant", "content": [
                {"type": "toolCall", "id": "c1", "name": "bash", "arguments": {"command": "cat x"}}]}),
            json!({"kind": "tool_result", "role": "toolResult", "toolCallId": "c1", "toolName": "bash",
                   "content": [{"type": "text", "text": dirty}], "isError": false}),
        ]);
        v.toggle_expanded();
        v.reply(Some("/job"), dirty);
        let rows = v.root.render(40);
        for row in &rows {
            assert!(visible_width(row) <= 40, "{row:?}");
            let plain = rpi_tui::strip_ansi(row);
            assert!(!plain.chars().any(|c| c.is_control()), "{row:?}");
            assert!(!row.contains("\x1b]"), "{row:?}");
        }
        let got = text(&v, 40);
        assert!(got.contains("     a    b\n     credend"), "{got}");
        assert!(got.contains("│ a    b"), "{got}");
    }

    #[test]
    fn replies_render_as_titled_cards() {
        let mut v = view();
        v.reply(Some("/status"), "agent: work on a/b");
        let got = text(&v, 40);
        assert_eq!(
            got,
            "   ╭─ /status ────────────────────────╮\n   │ agent: work on a/b               │\n   ╰──────────────────────────────────╯"
        );
    }
}
