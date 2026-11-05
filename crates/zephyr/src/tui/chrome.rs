//! The screen around the transcript: header, run line, queue strip and footer. Each is a pure
//! function of [`State`] and the clock, drawn by a [`Region`] component.

use super::conn::{AccountModels, Snapshot};
use super::overlays::{Complete, Kind, Overlay};
use super::theme::{glyph, Palette};
use rpi_tui::theme::Color;
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
    /// The open panel: model picker, status or help.
    pub overlay: Option<Overlay>,
    /// Slash autocomplete for the composer's text.
    pub complete: Option<Complete>,
    /// When the link first came up and when the last run finished (unix ms): the mascot's
    /// one-shots play from them.
    pub greeted: Option<i64>,
    pub finished: Option<i64>,
    /// The last `zephyr.models` answer, for completing `/model`.
    pub models: Vec<AccountModels>,
    /// The vault's entry names, for completing `/vault rm` and the like.
    pub vault: Vec<String>,
    /// Tokens in the model's context after the last reply (its usage), for `ctx N%`.
    /// shortcut: stale after /compact or /new until the next reply; ask rpi's `get_state`
    /// `context.tokens` if that misleads.
    pub context: Option<i64>,
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
            overlay: None,
            complete: None,
            greeted: None,
            finished: None,
            models: vec![],
            vault: vec![],
            context: None,
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

/// Below this width the header is one row, without the mascot.
const MASCOT_MIN_WIDTH: usize = 60;
/// The mascot's columns: an 11-column cloud, 3 for its wind or marks, and a 2-column gap.
const MASCOT_WIDTH: usize = 16;
/// One mascot frame: six a second, the TUI's redraw.
const FRAME_MS: i64 = 166;
/// How long the one-shots play: the hello on connecting, the cheer when a run ends.
const GREET_MS: i64 = 2_000;
const DONE_MS: i64 = 1_000;

/// How the mascot looks: what zephyr is doing, at a glance.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Mood {
    Idle,
    /// A run is going: the cloud leans into its wind and blows.
    Working,
    /// Paused, or not connected yet.
    Asleep,
    /// The connection is down, a reply failed, or the budget is spent.
    Troubled,
    /// Since this time (unix ms), on first connecting: the outline draws itself, then a hello.
    Greet(i64),
    /// Since this time, when a run has just finished: a happy face and a spark.
    Done(i64),
}

impl Mood {
    pub fn of(s: &State, now: i64) -> Mood {
        let spent = s
            .snapshot
            .paused
            .as_deref()
            .is_some_and(|p| p.starts_with("budget"));
        let playing = |at: Option<i64>, ms: i64| at.filter(|t| (0..ms).contains(&(now - t)));
        match &s.link {
            Link::Down { .. } => Mood::Troubled,
            Link::Connecting(_) => Mood::Asleep,
            Link::Up if spent || s.snapshot.failed_replies > 0 || s.retry.is_some() => {
                Mood::Troubled
            }
            Link::Up if s.snapshot.paused.is_some() && !s.snapshot.run.busy => Mood::Asleep,
            Link::Up => match (playing(s.finished, DONE_MS), playing(s.greeted, GREET_MS)) {
                (Some(t), _) => Mood::Done(t),
                (_, Some(t)) => Mood::Greet(t),
                _ if s.snapshot.run.busy => Mood::Working,
                _ => Mood::Idle,
            },
        }
    }
}

/// A glyph's colour and whether it is bold.
type Ink = (rpi_tui::theme::Color, bool);

/// The mascot's 14 cells by 3 rows; `None` is blank.
struct Sprite([[Option<(char, Ink)>; 14]; 3]);

impl Sprite {
    fn put(&mut self, row: usize, col: usize, text: &str, ink: Ink) {
        for (i, ch) in text.chars().enumerate().filter(|(_, ch)| *ch != ' ') {
            self.0[row][col + i] = Some((ch, ink));
        }
    }

    /// The outline, columns 0-10; `lean` shifts its top a cell downwind.
    fn body(&mut self, ink: Ink, lean: usize) {
        self.put(0, lean, "  .◠◠.◠◠.", ink);
        self.put(1, 0, "(", ink);
        self.put(1, 10, ")", ink);
        self.put(2, 0, " `-.___.-'", ink);
    }

    /// `face` (eye, mouth, eye) with the left eye at column `at`, cheeks just outside.
    fn face(&mut self, face: &str, at: usize, ink: Ink, cheeks: Option<Ink>) {
        self.put(1, at, face, ink);
        if let Some(cheek) = cheeks {
            self.put(1, at - 1, "◦", cheek);
            self.put(1, at + 5, "◦", cheek);
        }
    }
}

/// Wind leaving the cloud's side, a cell a frame; only single-width, unambiguous glyphs.
const WIND: [&str; 6] = ["≋  ", "~≋ ", "-~≋", "∙-~", " ∙-", "  ∙"];

fn blinking(tick: i64) -> bool {
    matches!(tick.rem_euclid(90), 0 | 30 | 45 | 47 | 75)
}

/// The idle face over a 15 s loop: blinks at 0, 5 and 12.5 s, a double blink at 7.5 s, and a
/// two-second glance downwind at 10 s.
fn idle_face(g: &mut Sprite, tick: i64, ink: Ink) {
    match tick.rem_euclid(90) {
        _ if blinking(tick) => g.face("- ᴗ -", 3, ink, None),
        60..72 => g.face("o ᴗ o", 4, ink, None),
        _ => g.face("o ᴗ o", 3, ink, None),
    }
}

/// Zephyr's mascot, a line-art cloud that puffs the west wind: three rows of [`MASCOT_WIDTH`]
/// columns, a pure function of the mood and the clock. Moods differ by glyphs, not only colour.
pub fn mascot(mood: Mood, p: &Palette, now: i64) -> [String; 3] {
    // Loops run on the wall clock; one-shots from their start.
    let tick = match mood {
        Mood::Greet(t) | Mood::Done(t) => (now - t).div_euclid(FRAME_MS),
        _ => now.div_euclid(FRAME_MS),
    };
    if !super::theme::unicode() {
        let ink = if mood == Mood::Asleep {
            p.cloud_asleep
        } else {
            p.cloud
        };
        return ascii_mascot(mood, tick).map(|l| ink.fg(&format!("{l:<MASCOT_WIDTH$}")));
    }
    let (cloud, text, dim) = ((p.cloud, true), (p.text, false), (p.dim, false));
    let (accent, cheeks) = ((p.accent, false), Some((p.cloud_blush, true)));
    let mut g = Sprite([[None; 14]; 3]);
    match mood {
        Mood::Idle => {
            g.body(cloud, 0);
            idle_face(&mut g, tick, text);
        }
        Mood::Working => {
            // One gust a second: a big "o" as it leaves, a fainter one trailing below.
            let at = tick.rem_euclid(6) as usize;
            g.body(cloud, 1);
            g.face(if at < 2 { "> o <" } else { "> ₒ <" }, 3, text, None);
            g.put(1, 11, WIND[at], accent);
            g.put(2, 11, &WIND[(at + 3) % 6].replace('≋', "~"), dim);
        }
        Mood::Asleep => {
            let ink = (p.cloud_asleep, false);
            g.body(ink, 0);
            g.face(
                if (tick / 9).rem_euclid(2) == 1 {
                    "- ˳ -"
                } else {
                    "- ₒ -"
                },
                3,
                text,
                None,
            );
            // A z a second, growing and drifting up.
            g.put(1, 11, "ᶻ", ink);
            g.put(
                0,
                12,
                ["", "z", "zZ"][(tick / 6).rem_euclid(3) as usize],
                ink,
            );
        }
        Mood::Troubled => {
            g.body(cloud, 0);
            let flinch = matches!(tick.rem_euclid(12), 6 | 8);
            g.face("ᵒ ᴖ ᵒ", 3 - usize::from(flinch), text, None);
            g.put(0, 11, "!", (p.error, false));
            g.put(2, 11, ". .", dim);
        }
        Mood::Greet(_) => {
            // From half drawn, the outline finishes left to right, then a hello and a wink.
            g.body(cloud, 0);
            let drawn = (6 + 2 * tick).clamp(0, 11) as usize;
            for row in &mut g.0 {
                row[drawn..11].fill(None);
            }
            match tick {
                3..11 => {
                    let wink = tick >= 6 && tick % 4 >= 2;
                    g.face(if wink { "^ ᴗ -" } else { "^ ᴗ ^" }, 3, text, cheeks);
                    let puff = ["~", "≋~", " ≋~", "  ≋"][((tick - 3) / 2).min(3) as usize];
                    g.put(1, 11, puff, accent);
                }
                11.. => idle_face(&mut g, 1, text),
                _ => {}
            }
        }
        Mood::Done(_) => {
            g.body(cloud, 0);
            if tick < 6 {
                g.face("^ ᴗ ^", 3, text, cheeks);
                let up = (tick / 2).clamp(0, 2) as usize;
                g.put(2 - up, 11 + up, "⋆", (p.warning, false));
            } else {
                idle_face(&mut g, 1, text);
            }
        }
    }
    g.0.map(|row| {
        let mut line: String = (row.iter())
            .map(|cell| match cell {
                Some((ch, (color, true))) => color.fg(&bold(&ch.to_string())),
                Some((ch, (color, false))) => color.fg(&ch.to_string()),
                None => " ".into(),
            })
            .collect();
        line.push_str("  ");
        line
    })
}

/// The mascot without UTF-8: the same cloud and moods in ASCII, 11 columns and its marks.
fn ascii_mascot(mood: Mood, tick: i64) -> [String; 3] {
    let (top, bottom) = ("  .--.--.  ", " `-.___.-' ");
    let mid = match mood {
        Mood::Working => {
            let wind = ["~  ", "~~ ", "~~~", " ~~", "  ~", "   "][tick.rem_euclid(6) as usize];
            format!(" ( > o < ) {wind}")
        }
        Mood::Asleep => {
            let z = ["", " z", " zZ"][(tick / 6).rem_euclid(3) as usize];
            return [format!("{top}{z}"), " ( - o - ) ".into(), bottom.into()];
        }
        Mood::Troubled => return [format!("{top}!"), " ( o n o ) ".into(), bottom.into()],
        Mood::Greet(_) if tick < 11 => " ( ^ u ^ ) ".into(),
        Mood::Done(_) if tick < 6 => " ( ^ u ^ ) ".into(),
        _ if blinking(tick) => " ( - u - ) ".into(),
        _ => " ( o u o ) ".into(),
    };
    [top.into(), mid, bottom.into()]
}

/// The cloud beside three rows: `zephyr v0.1.0` with the pause badge, `session · account ›
/// model`, then the channels with today's spend; then a rule. Narrower than
/// [`MASCOT_MIN_WIDTH`], one row without the cloud.
pub fn header(s: &State, p: &Palette, width: usize, now: i64) -> Vec<String> {
    if width < MASCOT_MIN_WIDTH {
        return compact_header(s, p, width);
    }
    let snap = &s.snapshot;
    let session = if snap.session.is_empty() {
        "main"
    } else {
        &snap.session
    };
    let mut name = p.accent.fg(&bold("zephyr"));
    if !snap.version.is_empty() {
        name.push_str(&p.dim.fg(&format!(" v{}", snap.version)));
    }
    let title = dotted(
        &[
            p.text.fg(session),
            match &s.link {
                Link::Connecting(_) => p.muted.fg(&format!("connecting{}", glyph("…"))),
                Link::Down { .. } => p.warning.fg(&format!("reconnecting{}", glyph("…"))),
                Link::Up => account_model(snap, p),
            },
        ],
        p,
    );
    let paused = paused_badge(s, p).unwrap_or_default();
    let mut channels = channel_dots(s, p);
    let mut right: Vec<String> = failed(s, p).into_iter().collect();
    if s.link == Link::Up {
        right.push(spend(snap, p));
    }
    let text = width - MASCOT_WIDTH;
    // The channels drop off from the last, then spend; failed replies stay.
    let status = loop {
        let (left, r) = (channels.join("  "), right.join("   "));
        if visible_width(&left) + visible_width(&r) + 3 <= text
            || (channels.is_empty() && right.len() <= 1)
        {
            break spread(&left, &format!("{r} "), text);
        }
        match channels.pop() {
            Some(_) => {}
            None => drop(right.pop()),
        }
    };
    let [m0, m1, m2] = mascot(Mood::of(s, now), p, now);
    let rule = format!(" {} ", glyph("─").repeat(width.saturating_sub(2)));
    vec![
        format!("{m0}{}", spread(&name, &format!("{paused} "), text)),
        format!("{m1}{}", spread(&title, "", text)),
        format!("{m2}{status}"),
        p.dim.fg(&rule),
    ]
}

/// `✗ 2 failed replies`, when the outbox has any.
fn failed(s: &State, p: &Palette) -> Option<String> {
    let n = s.snapshot.failed_replies;
    let what = if n == 1 { "reply" } else { "replies" };
    (n > 0).then(|| p.error.fg(&format!("{} {n} failed {what}", glyph("✗"))))
}

/// `account › model`, or the model alone without a pool.
fn account_model(snap: &Snapshot, p: &Palette) -> String {
    let model = snap
        .model
        .as_deref()
        .map(short_model)
        .unwrap_or("default model");
    let thinking = (snap.thinking.as_ref()).map_or(String::new(), |t| {
        p.muted.fg(&format!(" {} {t}", glyph("·")))
    });
    match &snap.account {
        Some(a) => format!(
            "{} {} {}{thinking}",
            p.text.fg(a),
            p.muted.fg(glyph("›")),
            p.text.fg(model)
        ),
        None => format!("{}{thinking}", p.text.fg(model)),
    }
}

/// `⏸ paused`, or `⏸ paused (budget)`, while the gateway holds new messages.
fn paused_badge(s: &State, p: &Palette) -> Option<String> {
    let why = s.snapshot.paused.as_ref().filter(|_| s.link == Link::Up)?;
    let label = if why.starts_with("budget") {
        "paused (budget)"
    } else {
        "paused"
    };
    Some(p.warning.fg(&format!("{} {label}", glyph("⏸"))))
}

/// `discord ●` per channel adapter; none while not connected.
fn channel_dots(s: &State, p: &Palette) -> Vec<String> {
    if s.link != Link::Up {
        return vec![];
    }
    s.snapshot
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
        .collect()
}

/// ` ≋ zephyr   main · account › model          ⏸ paused   discord ●  imessage ○`, then a rule.
fn compact_header(s: &State, p: &Palette, width: usize) -> Vec<String> {
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
        Link::Up => account_model(snap, p),
    };
    left.push_str(&p.muted.fg(&format!(" {} ", glyph("·"))));
    left.push_str(&where_);
    let paused = paused_badge(s, p);
    let mut channels = channel_dots(s, p);
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

/// The one-row status line: what zephyr is doing now, `≋ ready` when idle. Always one row.
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
                // The row is kept, so the transcript does not jump as runs start and end.
                (p.muted.fg(&format!("{} ready", glyph("≋"))), String::new())
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

/// Key hints, dropping off from the end first. Today's spend and failed replies are in the
/// header, so here only when the header is the compact one.
pub fn footer(s: &State, p: &Palette, width: usize, now: i64) -> Vec<String> {
    let key = |k: &str, what: &str| format!("{} {}", p.text.fg(k), p.muted.fg(what));
    let enter = glyph("⏎");
    let arrows = glyph("↑↓");
    // (hint, rank): when the row is short, the highest rank goes first.
    let mut hints = match (&s.overlay, &s.complete) {
        (Some(o), _) => {
            let (choose, pick) = match o.kind {
                Kind::Model => ("choose", Some("switch")),
                Kind::Help | Kind::History => ("choose", Some("insert")),
                Kind::Status => ("scroll", None),
                // The vault panel names its own keys on its last row.
                Kind::Vault => ("choose", None),
                // Once the sign-in runs there is nothing to choose, and the token goes in Enter.
                Kind::Login => match o.login.running() {
                    Some(_) => ("scroll", None),
                    None => ("choose", Some("sign in")),
                },
            };
            let mut hints = vec![(key(arrows, choose), 2)];
            hints.extend(pick.map(|pick| (key(enter, pick), 1)));
            if o.kind == Kind::Login && o.login.needs_input() {
                hints.clear();
                hints.push((key(enter, "send the code"), 1));
            } else if !matches!(o.kind, Kind::Status | Kind::Vault) {
                hints.push((p.muted.fg("type to filter"), 3));
            }
            hints.push((
                key(
                    "esc",
                    match o.kind {
                        // A running sign-in is given up on, not just closed.
                        Kind::Login if o.login.running().is_some() => "cancel",
                        _ => "close",
                    },
                ),
                0,
            ));
            hints
        }
        (None, Some(_)) => vec![
            (key(arrows, "choose"), 2),
            (key("tab", "complete"), 1),
            (key(enter, "run"), 3),
            (key("esc", "dismiss"), 0),
        ],
        _ if s.snapshot.run.busy && s.link == Link::Up => vec![
            (key("esc", "stop"), 0),
            (key(enter, "queue"), 2),
            (key("^O", "tools"), 3),
            (key("^L", "model"), 4),
            (key("^S", "status"), 5),
            (key("^D", "detach"), 1),
        ],
        _ => vec![
            (key(enter, "send"), 0),
            (key(&format!("{}{enter}", glyph("⇧")), "newline"), 4),
            (key("/", "commands"), 2),
            (key("^L", "model"), 3),
            (key("^S", "status"), 5),
            (key("^O", "tools"), 6),
            (key("?", "help"), 7),
            (key("^D", "detach"), 1),
        ],
    };
    if let Some((hint, until)) = &s.hint {
        if now < *until {
            hints = vec![(p.warning.fg(hint), 0)];
        }
    }

    let mut right = vec![];
    if width < MASCOT_MIN_WIDTH {
        right.extend(failed(s, p));
    }
    if let (Some(used), Some(window), Link::Up) = (s.context, s.snapshot.context_window, &s.link) {
        let share = used as f64 / window.max(1) as f64;
        let pct = format!("ctx {}%", (share * 100.0).round() as i64);
        right.push(gauge(p, share).fg(&pct));
    }
    if width < MASCOT_MIN_WIDTH && s.link == Link::Up {
        right.push(spend(&s.snapshot, p));
    }
    // Hints drop off first, then spend; failed replies stay.
    loop {
        let shown: Vec<String> = hints.iter().map(|(h, _)| h.clone()).collect();
        let left = format!("  {}", dotted(&shown, p));
        let r = format!("{} ", right.join("   "));
        let fits = visible_width(&left) + visible_width(&r) + 2 <= width;
        if fits || (hints.len() <= 1 && right.len() <= 1) {
            return vec![spread(&left, &r, width)];
        }
        if hints.len() > 1 {
            let last = (0..hints.len()).max_by_key(|&i| hints[i].1).unwrap_or(0);
            hints.remove(last);
        } else {
            right.pop();
        }
    }
}

/// `today 41.2k tok · $0.38 / $5`, in the warning colour from 80% of a limit, error from 100%.
pub fn spend(snap: &Snapshot, p: &Palette) -> String {
    let spend = &snap.spend;
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
    gauge(p, used).fg(&format!("{tokens} {} {cost}", glyph("·")))
}

/// How full something is: the warning colour from 80%, error from 100%.
fn gauge(p: &Palette, used: f64) -> Color {
    if used >= 1.0 {
        p.error
    } else if used >= 0.8 {
        p.warning
    } else {
        p.muted
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
pub fn short_model(id: &str) -> &str {
    id.rsplit('/')
        .next()
        .filter(|m| !m.is_empty())
        .unwrap_or(id)
}

#[cfg(test)]
mod tests {
    use super::super::conn;
    use super::super::theme::{DARK, PLAIN};
    use super::*;
    use crate::router::{ChannelState, Origin, Run, Spend, Waiting};

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
    fn header_puts_the_cloud_beside_model_channels_and_spend() {
        let s = up();
        let wide = plain(header(&s, &DARK, 120, NOW));
        assert_eq!(wide.len(), 4, "{wide:?}");
        for row in &wide {
            assert_eq!(rpi_tui::visible_width(row), 120, "{row:?}");
        }
        assert_eq!(wide[0].trim_end(), "  .◠◠.◠◠.       zephyr");
        assert_eq!(
            wide[1].trim_end(),
            "(  o ᴗ o  )     main · claude-code-1 › claude-opus-5-5"
        );
        assert!(
            wide[2].starts_with(" `-.___.-'      discord ●  imessage ◐  chat ○"),
            "{wide:?}"
        );
        assert!(
            wide[2].ends_with("today 41.2k tok · $0.38 / $5 "),
            "{wide:?}"
        );
        assert!(wide[3].trim().chars().all(|c| c == '─'));
        // Channels drop off from the last; spend stays.
        let narrow = plain(header(&s, &DARK, 70, NOW));
        assert!(
            narrow[2].contains("discord ●") && !narrow[2].contains("chat ○"),
            "{narrow:?}"
        );
        assert!(narrow[2].ends_with("$0.38 / $5 "), "{narrow:?}");
        // Narrower than the cloud allows: the one-row header.
        let tiny = plain(header(&s, &DARK, 59, NOW));
        assert_eq!(tiny.len(), 2, "{tiny:?}");
        assert!(
            tiny[0].starts_with(" ≋ zephyr   main · claude-code-1"),
            "{tiny:?}"
        );

        let mut paused = up();
        paused.snapshot.paused = Some("user".into());
        paused.snapshot.version = "0.1.0".into();
        let h = plain(header(&paused, &DARK, 80, NOW));
        assert!(h[0].starts_with("  .◠◠.◠◠.   z   zephyr v0.1.0 "), "{h:?}");
        assert!(h[0].trim_end().ends_with("⏸ paused"), "{h:?}");
        assert!(h[1].starts_with("(  - ˳ -  )ᶻ"), "an asleep cloud: {h:?}");

        let mut down = up();
        down.link = Link::Down {
            why: "refused".into(),
        };
        let h = plain(header(&down, &DARK, 80, NOW));
        assert!(h[1].contains("     main · reconnecting…"), "{h:?}");
        assert!(h[1].starts_with("(  ᵒ ᴖ ᵒ  ) "), "{h:?}");
        assert!(
            !h[2].contains("discord"),
            "stale channel dots are hidden: {h:?}"
        );
    }

    #[test]
    fn failed_replies_stay_visible_when_the_header_is_tight() {
        let mut s = up();
        s.snapshot.failed_replies = 2;
        for w in 60..70 {
            let h = plain(header(&s, &DARK, w, NOW));
            let f = plain(footer(&s, &DARK, w, NOW));
            assert!(
                h[2].contains("2 failed replies") || f[0].contains("2 failed replies"),
                "{w}: {h:?} {f:?}"
            );
            assert_eq!(rpi_tui::visible_width(&h[2]), w);
        }
        // With the one-row header the footer keeps them, before spend.
        for w in 30..60 {
            let f = plain(footer(&s, &DARK, w, NOW));
            assert!(f[0].contains("2 failed replies"), "{w}: {f:?}");
        }
    }

    #[test]
    fn the_cloud_shows_the_mood_and_plays_its_one_shots() {
        // Frames as the design prototype draws them (mascot-lab designs/b_glyphs.py).
        let at = |tick: i64| tick * FRAME_MS;
        for (mood, now, want) in [
            (
                Mood::Idle,
                at(28),
                ["  .◠◠.◠◠.", "(  o ᴗ o  )", " `-.___.-'"],
            ),
            (
                Mood::Idle,
                at(90),
                ["  .◠◠.◠◠.", "(  - ᴗ -  )", " `-.___.-'"],
            ),
            (
                Mood::Idle,
                at(60),
                ["  .◠◠.◠◠.", "(   o ᴗ o )", " `-.___.-'"],
            ),
            (
                Mood::Working,
                at(0),
                ["   .◠◠.◠◠.", "(  > o <  )≋", " `-.___.-' ∙-~"],
            ),
            (
                Mood::Working,
                at(4),
                ["   .◠◠.◠◠.", "(  > ₒ <  ) ∙-", " `-.___.-' ~~"],
            ),
            (
                Mood::Asleep,
                at(12),
                ["  .◠◠.◠◠.   zZ", "(  - ˳ -  )ᶻ", " `-.___.-'"],
            ),
            (
                Mood::Troubled,
                at(6),
                ["  .◠◠.◠◠.  !", "( ᵒ ᴖ ᵒ   )", " `-.___.-' . ."],
            ),
            (Mood::Greet(0), at(0), ["  .◠◠.", "(", " `-.__"]),
            (
                Mood::Greet(0),
                at(6),
                ["  .◠◠.◠◠.", "( ◦^ ᴗ -◦ )≋~", " `-.___.-'"],
            ),
            (
                Mood::Done(0),
                at(4),
                ["  .◠◠.◠◠.    ⋆", "( ◦^ ᴗ ^◦ )", " `-.___.-'"],
            ),
        ] {
            let got = plain(mascot(mood, &DARK, now).to_vec());
            let want = want.map(|w| format!("{w:<MASCOT_WIDTH$}"));
            assert_eq!(got, want, "{mood:?} at {now}");
        }
        // Under NO_COLOR the moods still differ, by their glyphs.
        let moods = [Mood::Idle, Mood::Working, Mood::Asleep, Mood::Troubled];
        let frames: Vec<_> = (moods.iter())
            .map(|m| plain(mascot(*m, &PLAIN, NOW).to_vec()))
            .collect();
        for (i, f) in frames.iter().enumerate() {
            assert!(
                !frames[..i].contains(f),
                "{:?} looks like another mood",
                moods[i]
            );
        }

        let mut s = up();
        assert_eq!(Mood::of(&s, NOW), Mood::Idle);
        s.greeted = Some(NOW - 500);
        assert_eq!(Mood::of(&s, NOW), Mood::Greet(NOW - 500));
        assert_eq!(Mood::of(&s, NOW + GREET_MS), Mood::Idle);
        s.snapshot.run.busy = true;
        assert_eq!(Mood::of(&s, NOW), Mood::Greet(NOW - 500));
        assert_eq!(Mood::of(&s, NOW + GREET_MS), Mood::Working);
        s.snapshot.run.busy = false;
        s.finished = Some(NOW);
        assert_eq!(Mood::of(&s, NOW + 500), Mood::Done(NOW));
        assert_eq!(Mood::of(&s, NOW + GREET_MS), Mood::Idle);
        // Trouble and sleep beat the one-shots.
        s.snapshot.paused = Some("user".into());
        assert_eq!(Mood::of(&s, NOW + 500), Mood::Asleep);
        s.snapshot.failed_replies = 1;
        assert_eq!(Mood::of(&s, NOW + 500), Mood::Troubled);
    }

    #[test]
    fn every_mascot_frame_is_three_rows_of_single_width_glyphs() {
        use unicode_width::UnicodeWidthChar;
        let moods = [
            Mood::Idle,
            Mood::Working,
            Mood::Asleep,
            Mood::Troubled,
            Mood::Greet(0),
            Mood::Done(0),
        ];
        // 90 frames: the idle loop, and past the end of each one-shot.
        for mood in moods {
            for tick in 0..90 {
                let rows = plain(mascot(mood, &DARK, tick * FRAME_MS).to_vec());
                assert_eq!(rows.len(), 3);
                for row in &rows {
                    assert_eq!(visible_width(row), MASCOT_WIDTH, "{mood:?} {tick}: {row:?}");
                    for ch in row.chars() {
                        // Ambiguous glyphs are two columns in CJK terminals.
                        assert!(
                            ch.width() == Some(1) && ch.width_cjk() == Some(1),
                            "{mood:?} {tick}: {ch:?} is wide or ambiguous"
                        );
                        assert_eq!(visible_width(&ch.to_string()), 1, "{ch:?}");
                    }
                }
                for row in ascii_mascot(mood, tick) {
                    assert!(
                        row.is_ascii() && row.len() <= 14,
                        "{mood:?} {tick}: {row:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn run_line_tells_every_state() {
        let line = |s: &State| plain(run_line(s, &DARK, 80, NOW)).concat();
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
        // Idle keeps its row, so the transcript stays put when a run starts.
        assert_eq!(line(&s).trim(), "≋ ready");
        let idle_rows = run_line(&s, &DARK, 80, NOW).len();
        s.snapshot.paused = Some("user".into());
        assert!(line(&s).contains("⏸ paused: new messages wait until /resume"));
        s.snapshot.paused = None;
        s.snapshot.run = Run {
            busy: true,
            since_unix_ms: Some(NOW - 12_400),
            origin: Some(Origin {
                channel: "discord".into(),
                ..Default::default()
            }),
            current_tool: Some("bash".into()),
            tools_done: 2,
        };
        assert_eq!(run_line(&s, &DARK, 80, NOW).len(), idle_rows);
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
            ..Default::default()
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
        let q = |channel: &str, preview: &str| Waiting {
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
    fn footer_keeps_to_hints_and_spend_is_coloured_by_the_budget() {
        let mut s = up();
        let f = plain(footer(&s, &DARK, 120, NOW)).remove(0);
        assert!(
            f.starts_with(
                "  ⏎ send · ⇧⏎ newline · / commands · ^L model · ^S status · ^O tools · ? help · ^D detach"
            ),
            "{f}"
        );
        assert!(!f.contains("today"), "spend is in the header: {f}");
        // With the one-row header, spend comes back here and hints make room.
        let narrow = plain(footer(&s, &DARK, 59, NOW)).remove(0);
        assert!(
            narrow.ends_with("today 41.2k tok · $0.38 / $5 "),
            "{narrow}"
        );
        assert!(narrow.starts_with("  ⏎ send · ^D detach  "), "{narrow}");
        assert_eq!(rpi_tui::visible_width(&narrow), 59);

        let colour = |s: &State| spend(&s.snapshot, &DARK);
        assert!(colour(&s).contains(&DARK.muted.to_fg()));
        s.snapshot.spend.cost = 4.2;
        assert!(colour(&s).starts_with(&DARK.warning.to_fg()));
        s.snapshot.spend.cost = 5.0;
        assert!(colour(&s).starts_with(&DARK.error.to_fg()));
        s.snapshot.failed_replies = 2;
        assert!(plain(header(&s, &DARK, 120, NOW))[2].contains("✗ 2 failed replies"));
        s.hint = Some(("press ^C again to detach".into(), NOW + 1));
        assert!(plain(footer(&s, &DARK, 120, NOW))[0].starts_with("  press ^C again to detach"));
    }

    #[test]
    fn the_footer_shows_how_full_the_context_is_and_the_header_the_thinking_level() {
        let mut s = up();
        s.context = Some(84_000);
        let f = plain(footer(&s, &DARK, 120, NOW)).remove(0);
        assert!(!f.contains("ctx"), "no window known: {f}");
        s.snapshot.context_window = Some(200_000);
        let f = plain(footer(&s, &DARK, 120, NOW)).remove(0);
        assert!(f.ends_with("ctx 42% "), "{f}");
        let ctx = |s: &State| footer(s, &DARK, 120, NOW).remove(0);
        assert!(ctx(&s).contains(&DARK.muted.fg("ctx 42%")));
        s.context = Some(170_000);
        assert!(ctx(&s).contains(&DARK.warning.fg("ctx 85%")));
        s.snapshot.thinking = Some("high".into());
        let h = plain(header(&s, &DARK, 120, NOW));
        assert!(
            h[1].contains("claude-code-1 › claude-opus-5-5 · high"),
            "{h:?}"
        );
    }

    #[test]
    fn footer_hints_follow_the_open_panel() {
        let mut s = up();
        let left = |s: &State, w| plain(footer(s, &DARK, w, NOW))[0].clone();
        s.overlay = Some(Overlay::new(Kind::Model));
        let f = left(&s, 120);
        assert!(
            f.starts_with("  ↑↓ choose · ⏎ switch · type to filter · esc close  "),
            "{f}"
        );
        assert!(
            left(&s, 59).starts_with("  ⏎ switch · esc close  "),
            "{}",
            left(&s, 59)
        );
        s.overlay = Some(Overlay::new(Kind::Status));
        assert!(left(&s, 120).starts_with("  ↑↓ scroll · esc close  "));
        s.overlay = Some(Overlay::new(Kind::Help));
        assert!(left(&s, 120).starts_with("  ↑↓ choose · ⏎ insert · type to filter · esc close"));
        // The sign-in picker closes; a sign-in already running is given up on.
        s.overlay = Some(Overlay::new(Kind::Login));
        assert!(
            left(&s, 120).starts_with("  ↑↓ choose · ⏎ sign in · type to filter · esc close"),
            "{}",
            left(&s, 120)
        );
        if let Some(o) = s.overlay.as_mut() {
            o.login.session = Some(conn::Login {
                id: "l1".into(),
                provider: "claude-code".into(),
                account: None,
                kind: "setup_token".into(),
            });
            o.login.status = Some(conn::LoginStatus {
                phase: "need_input".into(),
                ..Default::default()
            });
        }
        assert!(
            left(&s, 120).starts_with("  ⏎ send the code · esc cancel"),
            "{}",
            left(&s, 120)
        );
        s.overlay = None;
        s.complete = Some(Complete {
            items: super::super::overlays::slash("/new"),
            selected: 0,
        });
        let f = left(&s, 120);
        assert!(
            f.starts_with("  ↑↓ choose · tab complete · ⏎ run · esc dismiss  "),
            "{f}"
        );
    }
}
