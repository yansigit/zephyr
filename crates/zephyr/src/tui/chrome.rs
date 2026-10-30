//! The screen around the transcript: header, run line, queue strip and footer. Each is a pure
//! function of [`State`] and the clock, drawn by a [`Region`] component.

use super::conn::Snapshot;
use super::theme::{glyph, Palette};
use rpi_tui::{bold, truncate_to_width, visible_width, Component};
use std::any::Any;
use std::sync::{Arc, Mutex};

/// The connection, as the header and run line tell it.
#[derive(Debug, Clone, PartialEq)]
pub enum Link {
    /// Not connected yet; the last failed attempt's error, if any.
    Connecting(Option<String>),
    Up,
    /// Lost; `why` is the last error.
    Down {
        why: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Retry {
    pub attempt: u64,
    pub max: u64,
    pub until_ms: i64,
    pub error: String,
}

#[derive(Debug, Clone)]
pub struct State {
    pub link: Link,
    pub snapshot: Snapshot,
    pub retry: Option<Retry>,
    /// `/stop` was sent and the run has not settled yet.
    pub stopping: bool,
    /// Prompts typed while not connected; they go out on connect.
    pub unsent: Vec<String>,
    /// A one-off footer hint (`^C again to detach`), until this time.
    pub hint: Option<(String, i64)>,
}

impl Default for State {
    fn default() -> Self {
        State {
            link: Link::Connecting(None),
            snapshot: Snapshot::default(),
            retry: None,
            stopping: false,
            unsent: vec![],
            hint: None,
        }
    }
}

impl State {
    /// Whether frames must keep coming: a timer or the wind glyph is on screen.
    pub fn animated(&self) -> bool {
        self.link != Link::Up || self.snapshot.run.busy || self.retry.is_some()
    }
}

/// Draws one part of the chrome from the shared state.
pub struct Region {
    pub state: Arc<Mutex<State>>,
    pub p: Palette,
    pub draw: fn(&State, &Palette, usize, i64) -> Vec<String>,
}

impl Component for Region {
    fn render(&self, width: usize) -> Vec<String> {
        let now = crate::unix_millis() as i64;
        (self.draw)(&self.state.lock().unwrap(), &self.p, width, now)
    }
    fn invalidate(&self) {}
    fn as_any(&self) -> &dyn Any {
        self
    }
}

/// `left`, then `right` flush with the right edge; `left` is cut first when both do not fit.
pub fn spread(left: &str, right: &str, width: usize) -> String {
    let rw = visible_width(right);
    if rw + 2 > width {
        return truncate_to_width(left, width, glyph("…"));
    }
    let left = truncate_to_width(left, width - rw - 2, glyph("…"));
    let gap = width - rw - visible_width(&left);
    format!("{left}{}{right}", " ".repeat(gap))
}

/// Joins styled parts with a muted `·`.
fn dotted(parts: &[String], p: &Palette) -> String {
    parts.join(&p.muted.fg(&format!(" {} ", glyph("·"))))
}

/// ` ≋ zephyr   main · account › model          ⏸ paused   discord ●  imessage ○`, then a rule.
pub fn header(s: &State, p: &Palette, width: usize, _now: i64) -> Vec<String> {
    let snap = &s.snapshot;
    let session = if snap.session.is_empty() {
        "main"
    } else {
        &snap.session
    };
    let mut left = format!(
        " {}   {}",
        p.accent.fg(&bold(&format!("{} zephyr", glyph("≋")))),
        p.text.fg(session)
    );
    let where_ = match &s.link {
        Link::Connecting(_) => p.muted.fg(&format!("connecting{}", glyph("…"))),
        Link::Down { .. } => p.warning.fg(&format!("reconnecting{}", glyph("…"))),
        Link::Up => {
            let model = snap
                .model
                .as_deref()
                .map(short_model)
                .unwrap_or("default model");
            match &snap.account {
                Some(a) => format!(
                    "{} {} {}",
                    p.text.fg(a),
                    p.muted.fg(glyph("›")),
                    p.text.fg(model)
                ),
                None => p.text.fg(model),
            }
        }
    };
    left.push_str(&p.muted.fg(&format!(" {} ", glyph("·"))));
    left.push_str(&where_);

    let paused = snap
        .paused
        .as_ref()
        .filter(|_| s.link == Link::Up)
        .map(|why| {
            let label = if why.starts_with("budget") {
                "paused (budget)"
            } else {
                "paused"
            };
            p.warning.fg(&format!("{} {label}", glyph("⏸")))
        });
    let mut channels: Vec<String> = snap
        .channels
        .iter()
        .map(|c| {
            let (g, color) = match c.state.as_str() {
                "connected" => ("●", p.success),
                "reconnecting" => ("◐", p.warning),
                _ => ("○", p.muted),
            };
            format!("{} {}", p.muted.fg(&c.name), color.fg(glyph(g)))
        })
        .collect();
    if s.link != Link::Up {
        channels.clear();
    }
    // The right side drops off first, the last channel first; the pause badge stays.
    let right = loop {
        let right = paused
            .iter()
            .cloned()
            .chain((!channels.is_empty()).then(|| channels.join("  ")))
            .collect::<Vec<_>>()
            .join("   ");
        if channels.is_empty() || visible_width(&left) + visible_width(&right) + 4 <= width {
            break right;
        }
        channels.pop();
    };
    let top = if right.is_empty() {
        truncate_to_width(&left, width, glyph("…"))
    } else {
        spread(&left, &format!("{right} "), width)
    };
    let rule = format!(" {} ", glyph("─").repeat(width.saturating_sub(2)));
    vec![top, p.dim.fg(&rule)]
}

/// The one-row status line: what zephyr is doing now. Blank when idle and connected.
pub fn run_line(s: &State, p: &Palette, width: usize, now: i64) -> Vec<String> {
    let run = &s.snapshot.run;
    let (left, right) = match &s.link {
        Link::Connecting(why) => (
            format!(
                "{} {}",
                p.warning.fg(glyph("◐")),
                p.muted.fg(&format!(
                    "connecting to zephyr{}{}",
                    glyph("…"),
                    why.as_ref().map(|w| format!("  {w}")).unwrap_or_default()
                ))
            ),
            String::new(),
        ),
        Link::Down { why } => (
            format!(
                "{} {}",
                p.warning.fg(glyph("◐")),
                p.muted.fg(&format!("reconnecting{}  {why}", glyph("…")))
            ),
            p.muted.fg("your draft is kept"),
        ),
        Link::Up => {
            if let Some(r) = &s.retry {
                let secs = ((r.until_ms - now).max(0) + 999) / 1000;
                let what = format!("retry {}/{} in {secs}s", r.attempt, r.max);
                let mut parts = vec![p.warning.fg(&what)];
                if !r.error.is_empty() {
                    parts.push(p.muted.fg(r.error.lines().next().unwrap_or_default()));
                }
                (
                    format!("{} {}", p.warning.fg(wind(now)), dotted(&parts, p)),
                    p.muted.fg("esc to stop"),
                )
            } else if run.busy {
                let mut what = match &run.current_tool {
                    Some(t) => format!("running {t}"),
                    None => "thinking".to_string(),
                };
                if s.stopping {
                    what = format!("stopping{}", glyph("…"));
                }
                if let Some(o) = run.origin.as_ref().filter(|o| o.channel != "tui") {
                    what.push_str(&format!(" for {}", o.channel));
                }
                let mut parts = vec![p.text.fg(&what)];
                if let Some(since) = run.since_unix_ms {
                    parts.push(p.muted.fg(&clock((now - since).max(0) / 1000)));
                }
                match run.tools_done {
                    0 => {}
                    1 => parts.push(p.muted.fg("1 tool")),
                    n => parts.push(p.muted.fg(&format!("{n} tools"))),
                }
                let right = if s.stopping {
                    String::new()
                } else {
                    p.muted.fg("esc to stop")
                };
                (
                    format!("{} {}", p.accent.fg(wind(now)), dotted(&parts, p)),
                    right,
                )
            } else if s.snapshot.paused.is_some() {
                (
                    format!(
                        "{} {}",
                        p.warning.fg(glyph("⏸")),
                        p.muted.fg("paused: new messages wait until /resume")
                    ),
                    String::new(),
                )
            } else {
                (String::new(), String::new())
            }
        }
    };
    vec![spread(&format!(" {left}"), &format!("{right} "), width)]
}

/// Up to two rows of what waits behind the current run, `+n more` after. Empty when nothing waits.
pub fn queue_strip(s: &State, p: &Palette, width: usize, _now: i64) -> Vec<String> {
    let items: Vec<(String, &str)> = s
        .unsent
        .iter()
        .map(|t| ("you, not sent yet".to_string(), t.as_str()))
        .chain(s.snapshot.queue.iter().map(|q| {
            let who = if q.channel == "tui" {
                "you"
            } else {
                &q.channel
            };
            (who.to_string(), q.preview.as_str())
        }))
        .collect();
    if items.is_empty() {
        return vec![];
    }
    let label = format!(" queued ({}) {} ", items.len(), glyph("·"));
    let item = |(who, text): &(String, &str)| {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        format!(
            "{} {}",
            p.muted.fg(&format!("{who}:")),
            p.text.fg(&format!("{}{text}{}", glyph("“"), glyph("”")))
        )
    };
    let mut rows = vec![format!("{}{}", p.muted.fg(&label), item(&items[0]))];
    if let Some(second) = items.get(1) {
        let mut row = format!("{}{}", " ".repeat(visible_width(&label)), item(second));
        if items.len() > 2 {
            row.push_str(&p.muted.fg(&format!("  +{} more", items.len() - 2)));
        }
        rows.push(row);
    }
    rows.iter()
        .map(|r| truncate_to_width(r, width, glyph("…")))
        .collect()
}

/// Key hints on the left, today's spend on the right. Hints drop off from the end first.
pub fn footer(s: &State, p: &Palette, width: usize, now: i64) -> Vec<String> {
    let key = |k: &str, what: &str| format!("{} {}", p.text.fg(k), p.muted.fg(what));
    let enter = glyph("⏎");
    // (hint, rank): when the row is short, the highest rank goes first.
    let mut hints = if s.snapshot.run.busy && s.link == Link::Up {
        vec![
            (key("esc", "stop"), 0),
            (key(enter, "queue"), 2),
            (key("^O", "tools"), 3),
            (key("^T", "thinking"), 4),
            (key("^D", "detach"), 1),
        ]
    } else {
        vec![
            (key(enter, "send"), 0),
            (key(&format!("{}{enter}", glyph("⇧")), "newline"), 2),
            (key("^O", "tools"), 3),
            (key("^T", "thinking"), 4),
            (key("^D", "detach"), 1),
        ]
    };
    if let Some((hint, until)) = &s.hint {
        if now < *until {
            hints = vec![(p.warning.fg(hint), 0)];
        }
    }

    let spend = &s.snapshot.spend;
    let mut right = vec![];
    if s.snapshot.failed_replies > 0 {
        let n = s.snapshot.failed_replies;
        let what = if n == 1 { "reply" } else { "replies" };
        right.push(p.error.fg(&format!("{} {n} failed {what}", glyph("✗"))));
    }
    if s.link == Link::Up {
        let mut tokens = format!("today {}", tokens_text(spend.tokens));
        if let Some(l) = spend.daily_tokens_limit {
            tokens.push_str(&format!(" / {}", tokens_text(l)));
        }
        tokens.push_str(" tok");
        let mut cost = money(spend.cost);
        if let Some(l) = spend.daily_cost_limit {
            cost.push_str(&format!(" / {}", money(l)));
        }
        let used = [
            spend
                .daily_tokens_limit
                .map(|l| spend.tokens as f64 / l.max(1) as f64),
            spend
                .daily_cost_limit
                .map(|l| spend.cost / l.max(f64::EPSILON)),
        ]
        .into_iter()
        .flatten()
        .fold(0.0, f64::max);
        let color = if used >= 1.0 {
            p.error
        } else if used >= 0.8 {
            p.warning
        } else {
            p.muted
        };
        right.push(color.fg(&format!("{tokens} {} {cost}", glyph("·"))));
    }
    let right = format!("{} ", right.join("   "));
    loop {
        let shown: Vec<String> = hints.iter().map(|(h, _)| h.clone()).collect();
        let left = format!("  {}", dotted(&shown, p));
        if hints.len() <= 1 || visible_width(&left) + visible_width(&right) + 2 <= width {
            return vec![spread(&left, &right, width)];
        }
        let last = (0..hints.len()).max_by_key(|&i| hints[i].1).unwrap_or(0);
        hints.remove(last);
    }
}

/// The wind glyph cycle, about 6 frames a second.
fn wind(now: i64) -> &'static str {
    glyph(["∼", "≈", "≋"][(now / 166).rem_euclid(3) as usize])
}

fn clock(secs: i64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

fn tokens_text(n: i64) -> String {
    match n {
        n if n < 1000 => n.to_string(),
        n if n < 1_000_000 => format!("{:.1}k", n as f64 / 1000.0),
        n => format!("{:.1}M", n as f64 / 1_000_000.0),
    }
}

fn money(v: f64) -> String {
    if v.fract() == 0.0 && v >= 1.0 {
        format!("${v:.0}")
    } else {
        format!("${v:.2}")
    }
}

/// `provider/model` → `model`.
fn short_model(id: &str) -> &str {
    id.rsplit('/')
        .next()
        .filter(|m| !m.is_empty())
        .unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::super::conn::{ChannelState, Origin, Queued, Run, Spend};
    use super::super::theme::DARK;
    use super::*;

    const NOW: i64 = 1_793_368_920_000;

    fn plain(lines: Vec<String>) -> Vec<String> {
        lines
            .iter()
            .map(|l| rpi_tui::strip_ansi(l).to_string())
            .collect()
    }

    fn up() -> State {
        let ch = |name: &str, state: &str| ChannelState {
            name: name.into(),
            state: state.into(),
        };
        State {
            link: Link::Up,
            snapshot: Snapshot {
                session: "main".into(),
                account: Some("claude-code-1".into()),
                model: Some("claude-code/claude-opus-5-5".into()),
                channels: vec![
                    ch("discord", "connected"),
                    ch("imessage", "reconnecting"),
                    ch("chat", "off"),
                ],
                spend: Spend {
                    tokens: 41_200,
                    cost: 0.38,
                    daily_tokens_limit: None,
                    daily_cost_limit: Some(5.0),
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn header_shows_the_model_and_drops_channels_when_narrow() {
        let s = up();
        let wide = plain(header(&s, &DARK, 120, NOW));
        assert!(
            wide[0].starts_with(" ≋ zephyr   main · claude-code-1 › claude-opus-5-5 "),
            "{wide:?}"
        );
        assert!(
            wide[0].ends_with("discord ●  imessage ◐  chat ○ "),
            "{wide:?}"
        );
        assert_eq!(rpi_tui::visible_width(&wide[0]), 120);
        assert!(wide[1].trim().chars().all(|c| c == '─'));
        let narrow = plain(header(&s, &DARK, 80, NOW));
        assert!(narrow[0].ends_with("discord ●  imessage ◐ "), "{narrow:?}");
        assert!(narrow[0].contains("claude-opus-5-5"), "{narrow:?}");
        let tiny = plain(header(&s, &DARK, 60, NOW));
        assert!(!tiny[0].contains("discord"), "{tiny:?}");

        let mut paused = up();
        paused.snapshot.paused = Some("user".into());
        let narrow = plain(header(&paused, &DARK, 80, NOW));
        assert!(narrow[0].ends_with("⏸ paused   discord ● "), "{narrow:?}");

        let mut down = up();
        down.link = Link::Down {
            why: "refused".into(),
        };
        let h = plain(header(&down, &DARK, 80, NOW));
        assert!(h[0].contains("main · reconnecting…"), "{h:?}");
        assert!(
            !h[0].contains("discord"),
            "stale channel dots are hidden: {h:?}"
        );
    }

    #[test]
    fn run_line_tells_every_state() {
        let line = |s: &State| plain(run_line(s, &DARK, 80, NOW)).remove(0);
        let mut s = State::default();
        assert!(line(&s).contains("connecting to zephyr…"));
        s.link = Link::Connecting(Some("zephyr is not answering yet".into()));
        assert!(line(&s).contains("connecting to zephyr…  zephyr is not answering yet"));
        s.link = Link::Down {
            why: "connection refused".into(),
        };
        let l = line(&s);
        assert!(
            l.contains("reconnecting…  connection refused") && l.ends_with("your draft is kept "),
            "{l}"
        );

        let mut s = up();
        assert_eq!(line(&s).trim(), "", "idle is blank");
        s.snapshot.paused = Some("user".into());
        assert!(line(&s).contains("⏸ paused: new messages wait until /resume"));
        s.snapshot.paused = None;
        s.snapshot.run = Run {
            busy: true,
            since_unix_ms: Some(NOW - 12_400),
            origin: Some(Origin {
                channel: "discord".into(),
            }),
            current_tool: Some("bash".into()),
            tools_done: 2,
        };
        let l = line(&s);
        assert!(
            l.contains(" running bash for discord · 12s · 2 tools"),
            "{l}"
        );
        assert!(l.ends_with("esc to stop "), "{l}");
        assert_eq!(rpi_tui::visible_width(&l), 80);
        s.snapshot.run.current_tool = None;
        s.snapshot.run.origin = Some(Origin {
            channel: "tui".into(),
        });
        s.snapshot.run.since_unix_ms = Some(NOW - 64_000);
        assert!(
            line(&s).contains(" thinking · 1m 04s · 2 tools"),
            "{}",
            line(&s)
        );
        s.stopping = true;
        assert!(line(&s).contains("stopping…") && !line(&s).contains("esc to stop"));
        s.retry = Some(Retry {
            attempt: 2,
            max: 10,
            until_ms: NOW + 3_200,
            error: "429 too many requests".into(),
        });
        assert!(
            line(&s).contains("retry 2/10 in 4s · 429 too many requests"),
            "{}",
            line(&s)
        );
    }

    #[test]
    fn queue_strip_shows_two_rows_then_a_count() {
        let mut s = up();
        assert!(queue_strip(&s, &DARK, 80, NOW).is_empty());
        let q = |channel: &str, preview: &str| Queued {
            channel: channel.into(),
            preview: preview.into(),
            since_unix: 0,
        };
        s.snapshot.queue = vec![
            q("imessage", "are you around later?"),
            q("tui", "and then"),
            q("discord", "x"),
        ];
        s.unsent = vec!["typed offline".into()];
        let rows = plain(queue_strip(&s, &DARK, 80, NOW));
        assert_eq!(rows[0], " queued (4) · you, not sent yet: “typed offline”");
        assert_eq!(
            rows[1],
            "              imessage: “are you around later?”  +2 more"
        );
    }

    #[test]
    fn footer_colours_spend_and_drops_hints_when_narrow() {
        let mut s = up();
        let f = plain(footer(&s, &DARK, 120, NOW)).remove(0);
        assert!(
            f.starts_with("  ⏎ send · ⇧⏎ newline · ^O tools · ^T thinking · ^D detach"),
            "{f}"
        );
        assert!(f.ends_with("today 41.2k tok · $0.38 / $5 "), "{f}");
        let narrow = plain(footer(&s, &DARK, 60, NOW)).remove(0);
        assert!(
            narrow.ends_with("today 41.2k tok · $0.38 / $5 "),
            "{narrow}"
        );
        assert!(narrow.starts_with("  ⏎ send · ^D detach  "), "{narrow}");
        assert_eq!(rpi_tui::visible_width(&narrow), 60);

        let colour = |s: &State| footer(s, &DARK, 120, NOW).remove(0);
        assert!(colour(&s).contains(&DARK.muted.to_fg()));
        s.snapshot.spend.cost = 4.2;
        assert!(colour(&s).contains(&format!("{}today", DARK.warning.to_fg())));
        s.snapshot.spend.cost = 5.0;
        assert!(colour(&s).contains(&format!("{}today", DARK.error.to_fg())));
        s.snapshot.failed_replies = 2;
        assert!(plain(footer(&s, &DARK, 120, NOW))[0].contains("✗ 2 failed replies"));
        s.hint = Some(("press ^C again to detach".into(), NOW + 1));
        assert!(plain(footer(&s, &DARK, 120, NOW))[0].starts_with("  press ^C again to detach"));
    }
}
