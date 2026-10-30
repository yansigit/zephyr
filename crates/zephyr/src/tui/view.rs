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
    /// except a tool card right under its assistant text or another card. A user turn's band
    /// spans the gutter, so it is not put in one.
    fn push(&mut self, kind: Kind, item: Arc<dyn Component>, mark: Option<String>) {
        self.last_notice = None;
        let tight = kind == Kind::Card && matches!(self.last, Kind::Text | Kind::Card);
        if self.last != Kind::None && !tight {
            self.root.add_child(Arc::new(Line(String::new())));
        }
        if kind == Kind::User {
            self.root.add_child(item);
        } else {
            self.root.add_child(Arc::new(Gutter { mark, item }));
        }
        self.last = kind;
    }

    fn user(&mut self, m: &Value) {
        let text = m["text"]
            .as_str()
            .map(str::to_string)
            .unwrap_or_else(|| crate::rpc::message_text(m));
        let text = clean(&text);
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
        let turn = UserTurn {
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
            self.push(Kind::Text, Arc::new(Prose::new(a)), Some(mark));
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

/// A user turn: a shaded band across the transcript, a bar in the channel's colour down its
/// left edge, and the channel and time at the right of its first row while they fit.
struct UserTurn {
    who: String,
    when: Option<String>,
    color: Color,
    text: String,
    p: Palette,
}

impl Component for UserTurn {
    fn render(&self, width: usize) -> Vec<String> {
        let (p, c) = (&self.p, self.color);
        // A column outside the band on each side; inside, the bar, a space, the text, a space.
        let room = width.saturating_sub(5).max(1);
        // The label takes at most half the first row: the time goes first, then the channel is
        // cut short, and only when not even three columns are left is it dropped.
        let max = (room / 2).saturating_sub(2);
        let tail = self.when.as_ref().map(|t| format!(" {} {t}", glyph("·")));
        let (name, tail) = match tail {
            Some(t) if visible_width(&self.who) + visible_width(&t) <= max => (self.who.clone(), t),
            _ if visible_width(&self.who) <= max => (self.who.clone(), String::new()),
            _ if max >= 3 => (truncate_to_width(&self.who, max, glyph("…")), String::new()),
            _ => (String::new(), String::new()),
        };
        let beside = !name.is_empty();
        let label = if beside {
            format!("{}{tail}", bold(&name))
        } else {
            name
        };
        let label_w = visible_width(&label);
        let first_room = if beside { room - label_w - 2 } else { room };
        // wrap_words' first line is a prefix of the text; the rest wraps at the full width.
        let first = wrap_words(&self.text, first_room).swap_remove(0);
        let rest =
            self.text[first.len()..].trim_start_matches(|c: char| c.is_whitespace() && c != '\n');
        let rest = rest.strip_prefix('\n').unwrap_or(rest);
        let mut lines = vec![first];
        if !rest.is_empty() {
            lines.extend(wrap_words(rest, room));
        }
        let bar = c.fg(glyph("▌"));
        lines
            .iter()
            .enumerate()
            .map(|(i, line)| {
                let label = if i == 0 && beside { &label } else { "" };
                let pad = room.saturating_sub(visible_width(line) + visible_width(label));
                let band = format!(
                    "{bar} {}{}{} ",
                    p.text.fg(line),
                    " ".repeat(pad),
                    c.fg(label)
                );
                let row = format!(" {}", p.user_bg.bg(&band));
                // Only a band wider than the screen (a few columns, or a wide character in one)
                // is cut: truncate_to_width measures emoji sequences wider than the layout does.
                match visible_width(&row) > width {
                    true => truncate_to_width(&row, width, ""),
                    false => row,
                }
            })
            .collect()
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Wide enough that upstream wraps no prose: the logical lines of a reply.
const UNWRAPPED: usize = 4096;

/// An assistant reply wrapped at words. rpi-tui's markdown wraps by character, splitting words at
/// the edge; this draws the reply once unwrapped and wraps its prose itself, with list items
/// hanging under their text. Blocks drawn to the full width (code, rules) and tables keep
/// upstream's layout, as does any reply whose two drawings do not line up block for block.
struct Prose {
    inner: Arc<AssistantMessageComponent>,
    cache: Mutex<Option<Drawn>>,
}

/// `(width, upstream's lines, ours)` from the last render.
type Drawn = (usize, Vec<String>, Vec<String>);

impl Prose {
    fn new(inner: Arc<AssistantMessageComponent>) -> Self {
        Prose {
            inner,
            cache: Mutex::new(None),
        }
    }
}

impl Component for Prose {
    fn render(&self, width: usize) -> Vec<String> {
        let narrow = self.inner.render(width);
        // Nothing reached the edge, so nothing was wrapped.
        if narrow.iter().all(|l| visible_width(l) + 1 < width) {
            return narrow;
        }
        let mut cache = self.cache.lock().unwrap();
        if let Some((w, n, ours)) = cache.as_ref() {
            if *w == width && *n == narrow {
                return ours.clone();
            }
        }
        let ours = reflow(&narrow, &self.inner.render(UNWRAPPED), width);
        *cache = Some((width, narrow, ours.clone()));
        ours
    }
    fn invalidate(&self) {
        self.inner.invalidate();
    }
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// Upstream's `narrow` drawing, with each prose block replaced by its `wide` (unwrapped) lines
/// wrapped at words.
fn reflow(narrow: &[String], wide: &[String], width: usize) -> Vec<String> {
    let blocks = |lines: &[String]| -> Vec<Vec<String>> {
        lines
            .split(|l| l.is_empty())
            .map(<[String]>::to_vec)
            .collect()
    };
    let (n, w) = (blocks(narrow), blocks(wide));
    if n.len() != w.len() {
        return narrow.to_vec();
    }
    let prose = |block: &[String]| {
        block.iter().all(|l| {
            let plain = rpi_tui::strip_ansi(l);
            let rule = plain.contains('─') && plain.trim().chars().all(|c| c == '─');
            visible_width(&plain) < UNWRAPPED - 1 && !rule && !plain.contains(['│', '┃', '┌', '├'])
        })
    };
    let mut out = vec![];
    for (i, (n, w)) in n.into_iter().zip(w).enumerate() {
        if i > 0 {
            out.push(String::new());
        }
        if prose(&w) {
            out.extend(w.iter().flat_map(|l| wrap_ansi(l.trim_end(), width)));
        } else {
            out.extend(n);
        }
    }
    out
}

/// Wraps one styled line at words. Continuations hang under the text: past the leading
/// spaces, and past a list marker (`•`, `-`, `1.`) when there is one. Styles carry over.
fn wrap_ansi(line: &str, width: usize) -> Vec<String> {
    let plain = rpi_tui::strip_ansi(line);
    if visible_width(&plain) <= width {
        return vec![line.to_string()];
    }
    // Plain leading spaces only: inline code's padding space comes after its colour.
    let lead = line.len() - line.trim_start_matches(' ').len();
    let marker = plain[lead..].split_once(' ').map_or(0, |(m, _)| {
        let numbered = m.len() > 1
            && m.strip_suffix('.')
                .is_some_and(|n| n.chars().all(|c| c.is_ascii_digit()));
        match matches!(m, "•" | "◦" | "▪" | "-" | "*") || numbered {
            true => visible_width(m) + 1,
            false => 0,
        }
    });
    let hang = (visible_width(&plain[..lead]) + marker).min(width / 2);
    // An indent deeper than the hang shrinks to it, so the first line has the room of the rest.
    let line = &line[(visible_width(&plain[..lead]) + marker)
        .saturating_sub(hang)
        .min(lead)..];

    // Words and runs of spaces, each with the escapes inside it.
    let mut parts: Vec<(String, usize, bool)> = vec![];
    for (text, escape) in tokens(line) {
        let space = text == " ";
        match parts.last_mut() {
            Some(last) if escape || (last.2 == space && (last.1 > 0 || !space)) => {
                last.0.push_str(&text);
                last.1 += if escape { 0 } else { visible_width(&text) };
            }
            _ if escape => parts.push((text, 0, false)),
            _ => parts.push((text.clone(), visible_width(&text), space)),
        }
    }

    let mut lines = vec![];
    let mut cur = String::new();
    let mut used = 0;
    let mut style: Vec<String> = vec![];
    let mut gap: Option<(String, usize)> = None;
    let add = |cur: &mut String, text: &str, style: &mut Vec<String>| {
        cur.push_str(text);
        for (t, escape) in tokens(text) {
            match t.strip_prefix("\x1b[").and_then(|t| t.strip_suffix('m')) {
                Some("" | "0") => style.clear(),
                Some(_) if escape => style.push(t),
                _ => {}
            }
        }
    };
    let escapes = |t: &str| {
        tokens(t)
            .into_iter()
            .filter(|(_, e)| *e)
            .map(|(t, _)| t)
            .collect::<String>()
    };
    for (text, w, space) in parts {
        if space {
            gap = Some((text, w));
            continue;
        }
        let room = if lines.is_empty() {
            width
        } else {
            width - hang
        };
        let spacing = gap.as_ref().map_or(0, |g| g.1);
        if used > 0 && used + spacing + w > room {
            // Break at the space: its escapes stay, its blanks go.
            if let Some((g, _)) = gap.take() {
                add(&mut cur, &escapes(&g), &mut style);
            }
            if !style.is_empty() {
                cur.push_str("\x1b[0m");
            }
            lines.push(std::mem::take(&mut cur));
            cur = format!("{}{}", " ".repeat(hang), style.concat());
            used = 0;
        } else if let Some((g, gw)) = gap.take() {
            add(&mut cur, &g, &mut style);
            used += gw;
        }
        let room = if lines.is_empty() {
            width
        } else {
            width - hang
        };
        if w > room.saturating_sub(used) {
            // Only at a line's start (else it broke above): a word wider than the line is split
            // where it must be, after any leading spaces.
            let room = room.saturating_sub(used).min(width - hang).max(1);
            let pieces = rpi_tui::wrap_text_with_ansi(&text, room);
            let last = pieces.len().saturating_sub(1);
            for (k, piece) in pieces.into_iter().enumerate() {
                cur.push_str(&piece);
                used = visible_width(&piece);
                if k < last {
                    lines.push(std::mem::take(&mut cur) + "\x1b[0m");
                    cur = " ".repeat(hang);
                }
            }
            add(&mut String::new(), &text, &mut style);
            continue;
        }
        add(&mut cur, &text, &mut style);
        used += w;
    }
    if let Some((g, _)) = gap {
        cur.push_str(&escapes(&g));
    }
    lines.push(cur);
    lines
}

/// A styled line as `(text, is_escape)`: each visible character on its own, each escape
/// sequence (CSI, or OSC and the like up to BEL or ST) whole.
fn tokens(line: &str) -> Vec<(String, bool)> {
    let mut out = vec![];
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\x1b' {
            out.push((c.to_string(), false));
            continue;
        }
        let mut seq = String::from(c);
        match chars.next() {
            Some('[') => {
                seq.push('[');
                for e in chars.by_ref() {
                    seq.push(e);
                    if ('@'..='~').contains(&e) {
                        break;
                    }
                }
            }
            Some(k) => {
                seq.push(k);
                while let Some(e) = chars.next() {
                    seq.push(e);
                    if e == '\x07' {
                        break;
                    }
                    if e == '\x1b' && chars.peek() == Some(&'\\') {
                        seq.extend(chars.next());
                        break;
                    }
                }
            }
            None => {}
        }
        out.push((seq, true));
    }
    out
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
                .unwrap_or(match fit {
                    0 => rest.chars().next().map_or(1, char::len_utf8),
                    fit => fit,
                });
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

/// A reply from zephyr itself (`/status`, `/model`, job output): its title, then its lines on
/// a dim rule.
struct SystemCard {
    title: String,
    text: String,
    p: Palette,
}

impl Component for SystemCard {
    fn render(&self, width: usize) -> Vec<String> {
        let p = &self.p;
        let title = truncate_to_width(&self.title, width, glyph("…"));
        let mut lines = vec![p.muted.fg(&bold(&title))];
        // Replies are plain lines (`/status` output), not markdown.
        let body = self
            .text
            .lines()
            .flat_map(|l| wrap_words(l, width.saturating_sub(2).max(1)));
        for row in body {
            lines.push(format!("{} {}", p.dim.fg(glyph("│")), p.text.fg(&row)));
        }
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
    fn history_maps_to_bands_tool_groups_and_gutter_marks() {
        let mut v = view();
        v.replay(&history());
        let got = text(&v, 80);
        let want = "   ───────────────────────────── Fri 30 Oct 2026 ─────────────────────────────

 ▌ remind me to renew the domain on friday                     discord · 14:02

 ≋ Done. I scheduled it for Fri 09:00.
   ▸ schedule  renew domain                                              ✓ 0.2s

 ▌ what's in deploy?                                               you · 14:05

   ▸ 2 tools  bash, read                                                 ✗ 0.4s
   ╰ ▸ read  deploy/missing                                              ✗ 0.3s
       ENOENT: no such file

 ≋ Two files.

 ▌ daily brief                                                    cron · 14:05";
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
        assert!(
            got.starts_with(" ▌ ping ") && got.contains(" imessage\n\n"),
            "{got}"
        );
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

    fn turn(who: &str, text: &str) -> UserTurn {
        UserTurn {
            who: who.into(),
            when: Some("14:02".into()),
            color: super::super::theme::DARK.discord,
            text: text.into(),
            p: super::super::theme::DARK,
        }
    }

    fn plain(rows: &[String]) -> Vec<String> {
        rows.iter().map(|l| rpi_tui::strip_ansi(l)).collect()
    }

    #[test]
    fn user_turns_are_bands_wrapped_at_words_with_the_label_beside() {
        let long =
            "please check whether the nightly backup finished and tell me which files changed";
        let rows = turn("discord", long).render(60);
        // A rectangle one column in from each edge; continuations hang under the text.
        assert_eq!(
            plain(&rows),
            [
                " ▌ please check whether the nightly        discord · 14:02 ",
                " ▌ backup finished and tell me which files changed         ",
            ]
        );
        assert!(
            rows.iter().all(|r| r.contains("\x1b[48;2;40;48;64m")),
            "{rows:?}"
        );
        // Lines the user broke stay broken.
        assert_eq!(
            plain(&turn("you", "one\n\ntwo").render(40)),
            [
                " ▌ one                     you · 14:02 ",
                " ▌                                     ",
                " ▌ two                                 "
            ]
        );
    }

    #[test]
    fn user_turns_never_overflow() {
        let family = "👨\u{200d}👩\u{200d}👧\u{200d}👦";
        let emoji = format!("👍🏽 family {family} flags 🇰🇷🇯🇵 ok");
        let families = format!("{}end", format!("{family} ").repeat(12));
        let texts = [
            "hi",
            "",
            "한국어 단어 줄바꿈 한국어 단어 줄바꿈",
            &"x".repeat(300),
            &emoji,
            &families,
        ];
        for who in ["discord", &"y".repeat(90)] {
            for text in texts {
                let want: Vec<char> = text.chars().filter(|c| !c.is_whitespace()).collect();
                for w in 1..=200 {
                    let rows = turn(who, text).render(w);
                    for row in &rows {
                        assert!(visible_width(row) <= w, "{w}: {row:?}");
                        // A square band, unless a wide character cannot fit a one-column line.
                        if w >= 7 {
                            assert_eq!(visible_width(row), w - 1, "{w}: {row:?}");
                        }
                    }
                    if w < 7 {
                        continue;
                    }
                    let rows = plain(&rows);
                    if who == "discord" && w >= 40 {
                        assert!(rows[0].ends_with("discord · 14:02 "), "{w}: {rows:#?}");
                    }
                    // Every character of the text reaches a row, in order.
                    let mut got = rows.iter().flat_map(|r| r.chars());
                    for c in &want {
                        assert!(got.any(|g| g == *c), "{w}: {c:?} lost from {rows:#?}");
                    }
                }
            }
        }
        // Wide characters wrap whole and the band stays square.
        let rows = plain(&turn("discord", "한국어 단어 줄바꿈 한국어 단어 줄바꿈").render(40));
        assert!(rows.iter().all(|r| visible_width(r) == 39), "{rows:#?}");
        assert!(rows[0].ends_with("discord · 14:02 "), "{rows:#?}");
    }

    #[test]
    fn narrow_bands_drop_the_time_then_shorten_the_channel() {
        let first = |w| plain(&turn("discord", "hi").render(w)).swap_remove(0);
        assert!(first(40).ends_with(" discord · 14:02 "), "{:?}", first(40));
        assert!(first(30).ends_with(" discord "), "{:?}", first(30));
        assert!(first(16).ends_with(" di… "), "{:?}", first(16));
        assert_eq!(first(12), " ▌ hi      ");
        // Space that is not a line break is trimmed after a wrap, so no blank row appears.
        for text in ["hello\u{a0}\nworld", "hi\u{3000}\nthere"] {
            let rows = plain(&turn("discord", text).render(40));
            assert_eq!(rows.len(), 2, "{rows:#?}");
        }
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

    /// The reply as the transcript draws it, plain, without the gutter.
    fn prose(text: &str, width: usize) -> Vec<String> {
        let a = Arc::new(AssistantMessageComponent::new(AssistantMessageOptions {
            output_pad: 0,
            ..Default::default()
        }));
        a.update_blocks(&[AssistantBlock::Text(text.into())]);
        let lines = Prose::new(a).render(width);
        for l in &lines {
            assert!(visible_width(l) <= width, "{l:?} is wider than {width}");
        }
        lines
            .iter()
            .map(|l| rpi_tui::strip_ansi(l).trim_end().to_string())
            .collect()
    }

    #[test]
    fn replies_wrap_at_words_with_lists_hanging_and_code_kept() {
        let text = "I can read files in `/zephyr`, run bash commands (including network requests like curl), and look up documentation.\n\n- **read**: reads text and image files and also some other things that are long\n- second item\n\n```\nlet x = \"a very long line of code that will certainly need to wrap somewhere\";\n```";
        let got = prose(text, 40);
        assert_eq!(
            got[..4],
            [
                "I can read files in  /zephyr , run bash",
                "commands (including network requests",
                "like curl), and look up documentation.",
                ""
            ],
            "{got:#?}"
        );
        assert_eq!(
            got[4..8],
            [
                "  • read: reads text and image files and",
                "    also some other things that are long",
                "  • second item",
                ""
            ],
            "{got:#?}"
        );
        // Code keeps upstream's layout: its own background, wrapped where it must be.
        assert!(
            got[8].contains("let x = \"a very long line of code"),
            "{got:#?}"
        );
    }

    #[test]
    fn a_style_broken_across_lines_carries_on() {
        let lines = wrap_ansi("say \x1b[1mthis bold phrase\x1b[22m done", 12);
        let plain: Vec<String> = lines.iter().map(|l| rpi_tui::strip_ansi(l)).collect();
        assert_eq!(plain, ["say this", "bold phrase", "done"]);
        assert!(lines[0].ends_with("\x1b[0m"), "{lines:?}");
        assert!(lines[1].starts_with("\x1b[1mbold"), "{lines:?}");
        // The styles so far are replayed, so bold that ended stays ended.
        assert!(lines[2].ends_with("\x1b[22mdone"), "{lines:?}");
        // Inline code's padding is not an indent.
        let code: Vec<String> = wrap_ansi("\x1b[48;5;236m /exit \x1b[49m came through as text", 14)
            .iter()
            .map(|l| rpi_tui::strip_ansi(l))
            .collect();
        assert_eq!(code, [" /exit  came", "through as", "text"]);
        // A list item hangs under its text; a word wider than the line is split.
        let item: Vec<String> = wrap_ansi("  • one two three", 10)
            .iter()
            .map(|l| rpi_tui::strip_ansi(l))
            .collect();
        assert_eq!(item, ["  • one", "    two", "    three"]);
        let long: Vec<String> = wrap_ansi("a abcdefghijkl", 6)
            .iter()
            .map(|l| rpi_tui::strip_ansi(l))
            .collect();
        assert_eq!(long, ["a", "abcdef", "ghijkl"]);
    }

    #[test]
    fn review_edge_cases_stay_inside_the_width() {
        // A rule is one row, not a word split in two.
        let rule = prose("above\n\n---\n\nbelow", 76);
        assert_eq!(
            rule.iter().filter(|l| l.starts_with('─')).count(),
            1,
            "{rule:#?}"
        );
        // A long token after leading spaces is split, not left overflowing.
        for l in wrap_ansi("    abcdefghijklmnopqrst", 10) {
            assert!(visible_width(&l) <= 10, "{l:?}");
        }
        let item = prose(
            "- item one\n\n  https://example.com/a/very/long/path/that/overflows/the/line",
            30,
        );
        assert!(item.iter().any(|l| l.contains("https://")), "{item:#?}");
        // An indent deeper than half the line shrinks to the hang, so words keep whole.
        let deep: Vec<String> = wrap_ansi("          ex", 11)
            .iter()
            .map(|l| rpi_tui::strip_ansi(l))
            .collect();
        assert_eq!(deep, ["     ex"]);
        for l in wrap_ansi("          loyha", 9) {
            assert!(visible_width(&l) <= 9, "{l:?}");
        }
        // A wide first character at width 1 does not panic.
        assert_eq!(wrap_words("한국어", 1), ["한", "국", "어"]);
    }

    #[test]
    fn a_reply_without_long_lines_is_drawn_as_upstream_draws_it() {
        let a = Arc::new(AssistantMessageComponent::new(AssistantMessageOptions {
            output_pad: 0,
            ..Default::default()
        }));
        a.update_blocks(&[AssistantBlock::Text(
            "short **bold** and `code`\n\n| a | b |\n|---|---|\n| 1 | 2 |".into(),
        )]);
        assert_eq!(Prose::new(a.clone()).render(60), a.render(60));
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
        assert_eq!(got, "   /status\n   │ agent: work on a/b");
    }
}
