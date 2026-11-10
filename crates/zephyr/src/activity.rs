//! `/activity` (DESIGN §4.19): the daemon's own record of what ran, what left the machine, what
//! waited and what was refused, read from root's `state.db` and rendered here, never by the model.
//! Every text is scrubbed as it is read, with the vault and the facts as they are now.

use crate::config::Config;
use crate::gate::Scrub;
use crate::settings::one_line;
use crate::store::{Cursor, Record, Store};
use jiff::tz::TimeZone;

/// The kinds of record, as `/activity KIND` names them.
pub const KINDS: [&str; 9] = [
    "out", "gate", "refused", "setting", "proposal", "job", "task", "process", "tool",
];

pub const USAGE: &str = "[zephyr] usage: /activity [today|6h|7d] \
    [out|gate|refused|setting|proposal|job|task|process|tool|grants]";

/// The most records a chat answer shows; one more is asked for, to know there are more.
const CHAT_LIMIT: u32 = 30;
const DAY: i64 = 86_400;

/// How far back to read: from local midnight, or the last so many seconds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Window {
    Today,
    /// Seconds, 1 h to 90 d.
    Last(i64),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Query {
    pub window: Window,
    /// A [`KINDS`] entry or `grants`; `None` is every kind but `tool`.
    pub kind: Option<&'static str>,
    pub before: Option<Cursor>,
    pub limit: u32,
}

impl Query {
    /// The chat form: up to two words, a time and a kind, in either order, any case.
    pub fn words(arg: &str) -> Option<Query> {
        let mut q = Query {
            window: Window::Last(DAY),
            kind: None,
            before: None,
            limit: CHAT_LIMIT,
        };
        let words: Vec<String> = arg
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect();
        if words.len() > 2 {
            return None;
        }
        let (mut timed, mut kinded) = (false, false);
        for w in &words {
            if let Some(window) = window(w) {
                if std::mem::replace(&mut timed, true) {
                    return None;
                }
                q.window = window;
            } else if let Some(kind) = kind(w) {
                if std::mem::replace(&mut kinded, true) {
                    return None;
                }
                q.kind = Some(kind);
            } else {
                return None;
            }
        }
        Some(q)
    }
}

/// `today`, or `Nh`/`Nd` from 1 h to 90 d.
fn window(w: &str) -> Option<Window> {
    if w == "today" {
        return Some(Window::Today);
    }
    if !w.ends_with(['h', 'd']) {
        return None;
    }
    let secs = i64::try_from(crate::settings::duration(w).ok()?.as_secs()).ok()?;
    (3600..=90 * DAY)
        .contains(&secs)
        .then_some(Window::Last(secs))
}

/// A kind, singular or plural, or `grants`.
fn kind(w: &str) -> Option<&'static str> {
    match w {
        "grants" => Some("grants"),
        "processes" => Some("process"),
        "process" => Some("process"),
        _ => (KINDS.iter().copied()).find(|k| *k != "process" && (w == *k || w == format!("{k}s"))),
    }
}

/// One record as every client shows it: `line` is the chat line, so no client formats anything.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Entry {
    pub at: i64,
    pub kind: String,
    pub subject: String,
    pub status: String,
    pub text: String,
    pub line: String,
    /// The row comes from rpi's event stream, which a hostile agent can forge (DESIGN §4.4).
    pub stream: bool,
}

/// Where `w` starts, in unix seconds.
pub fn since(w: &Window, now: i64, tz: &TimeZone) -> i64 {
    match w {
        Window::Last(secs) => now - secs,
        Window::Today => jiff::Timestamp::from_second(now)
            .ok()
            .and_then(|t| t.to_zoned(tz.clone()).start_of_day().ok())
            .map_or(now - DAY, |z| z.timestamp().as_second()),
    }
}

/// `r` scrubbed and cut, with its line: `WHEN KIND SUBJECT[ STATUS]: TEXT`.
pub(crate) fn entry(r: &Record, scrub: &Scrub, now: i64, tz: &TimeZone) -> Entry {
    let audit = r.src == "audit";
    let text = one_line(&scrub.text(&r.text), 200);
    // `audit.session` is named by gate socket requests, which the agent can forge.
    let subject = match audit {
        true => one_line(&session(scrub, &r.subject), 40),
        false => one_line(&r.subject, 40),
    };
    let status = match (r.kind.as_str(), r.status.as_str()) {
        ("setting", _) | (_, "started") => String::new(),
        ("refused", "refused") => " by the policy".into(),
        ("refused", "denied") => " by the gate".into(),
        (_, s) => format!(" {}", one_line(s, 40)),
    };
    let line = format!(
        "{} {} {subject}{status}: {text}",
        when(r.at, now, tz),
        r.kind
    );
    Entry {
        at: r.at,
        kind: r.kind.clone(),
        subject,
        status: r.status.clone(),
        text,
        line,
        stream: audit && (r.kind == "out" || r.kind == "tool" || r.status == "refused"),
    }
}

/// An audit row's session, scrubbed. When the vault or the facts cannot be read, one of the
/// daemon's own names (`main`, `task-12`, `job-3-88`) still shows, since it holds no value;
/// anything else stays withheld.
fn session(scrub: &Scrub, s: &str) -> String {
    let clean = scrub.text(s);
    let withheld = clean.starts_with("[output withheld:") || clean.starts_with("[withheld:");
    let number = |n: &str| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit());
    let own = s == "main"
        || s.split_once('-').is_some_and(|(word, ids)| {
            matches!(word, "task" | "job" | "action") && ids.split('-').all(number)
        });
    match withheld && own {
        true => s.into(),
        false => clean,
    }
}

/// `HH:MM` on `now`'s local day, else `MM-DD HH:MM`.
fn when(at: i64, now: i64, tz: &TimeZone) -> String {
    let local = |t: i64| jiff::Timestamp::from_second(t).map(|t| t.to_zoned(tz.clone()));
    match (local(at), local(now)) {
        (Ok(a), Ok(n)) if a.date() == n.date() => a.strftime("%H:%M").to_string(),
        (Ok(a), _) => a.strftime("%m-%d %H:%M").to_string(),
        _ => String::new(),
    }
}

/// How the header and the empty answer name the window.
fn window_words(w: &Window) -> String {
    match *w {
        Window::Today => "since 00:00 today".into(),
        Window::Last(DAY) => "in the last 24 h".into(),
        Window::Last(s) if s % DAY == 0 => format!("in the last {} d", s / DAY),
        Window::Last(s) => format!("in the last {} h", s / 3600),
    }
}

fn noun(kind: Option<&str>) -> &'static str {
    match kind {
        Some("out") => "outbound commands",
        Some("gate") => "gate decisions",
        Some("refused") => "refusals",
        Some("setting") => "setting changes",
        Some("proposal") => "proposals",
        Some("job") => "job runs",
        Some("task") => "tasks",
        Some("process") => "processes",
        Some("tool") => "tool calls",
        _ => "activity",
    }
}

/// The whole reply to `/activity ARG`, for the owner on any channel and the agent's `gateway`.
pub fn chat(cfg: &Config, store: &Store, arg: &str, now: i64) -> String {
    let Some(q) = Query::words(arg) else {
        return USAGE.into();
    };
    if q.kind == Some("grants") {
        return USAGE.into();
    }
    let tz = crate::scheduler::zone(cfg).unwrap_or(TimeZone::UTC);
    let kinds: Vec<&str> = q.kind.into_iter().collect();
    let mut records = match store.activity(since(&q.window, now, &tz), &kinds, None, q.limit + 1) {
        Ok(r) => r,
        Err(e) => return format!("[zephyr] reading the activity failed: {e}"),
    };
    let window = window_words(&q.window);
    if records.is_empty() {
        return format!("[zephyr] nothing recorded {window}");
    }
    let more = records.len() > q.limit as usize;
    records.truncate(q.limit as usize);
    let scrub = Scrub::load(cfg);
    let mut out = vec![format!("[zephyr] {} {window}, newest first:", noun(q.kind))];
    out.extend(records.iter().map(|r| entry(r, &scrub, now, &tz).line));
    if more {
        out.push(format!(
            "(only the newest {}; /activity KIND or a shorter time narrows it)",
            q.limit
        ));
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Record;
    use jiff::tz::TimeZone;

    fn cfg(name: &str) -> (Config, Store) {
        let dir = crate::test_dir(name);
        let cfg: Config =
            toml::from_str(&format!("rpi = '/r'\nhome = {dir:?}\ntimezone = 'UTC'\n")).unwrap();
        std::fs::create_dir_all(cfg.state_dir()).unwrap();
        let store = Store::open(&cfg.db_path()).unwrap();
        (cfg, store)
    }

    fn rec(at: i64, src: &str, kind: &str, subject: &str, status: &str, text: &str) -> Record {
        Record {
            at,
            src: src.into(),
            id: 1,
            kind: kind.into(),
            subject: subject.into(),
            status: status.into(),
            text: text.into(),
        }
    }

    /// 2025-11-10 14:30 UTC.
    const NOW: i64 = 1_762_785_000;

    fn at(hh: i64, mm: i64) -> i64 {
        NOW - NOW % 86_400 + hh * 3600 + mm * 60
    }

    #[test]
    fn words_parse_as_the_spec_says() {
        let q = Query::words("").unwrap();
        assert_eq!(
            (q.window, q.kind, q.limit),
            (Window::Last(86_400), None, 30)
        );
        assert_eq!(q.before, None);
        assert_eq!(Query::words("today").unwrap().window, Window::Today);
        assert_eq!(Query::words("6h").unwrap().window, Window::Last(6 * 3600));
        assert_eq!(
            Query::words("90d").unwrap().window,
            Window::Last(90 * 86_400)
        );
        assert_eq!(
            Query::words("2160h").unwrap().window,
            Window::Last(90 * 86_400)
        );
        for (arg, window, kind) in [
            ("tasks 7d", Window::Last(7 * 86_400), "task"),
            ("7d tasks", Window::Last(7 * 86_400), "task"),
            ("7D Tasks", Window::Last(7 * 86_400), "task"),
            ("Today out", Window::Today, "out"),
            ("processes", Window::Last(86_400), "process"),
            ("grants", Window::Last(86_400), "grants"),
            ("tool", Window::Last(86_400), "tool"),
        ] {
            let q = Query::words(arg).unwrap_or_else(|| panic!("{arg}"));
            assert_eq!((q.window, q.kind), (window, Some(kind)), "{arg}");
        }
        assert_eq!(Query::words("Today").unwrap().window, Window::Today);
        for bad in [
            "91d",
            "0h",
            "30m",
            "60s",
            "6h 7d",
            "task job",
            "6h task x",
            "nonsense",
            "processs",
        ] {
            assert!(Query::words(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn each_kind_renders_its_golden_line() {
        let (cfg, _store) = cfg("activity-golden");
        let scrub = Scrub::load(&cfg);
        let utc = TimeZone::UTC;
        for (r, line) in [
            (
                rec(
                    at(14, 5),
                    "audit",
                    "gate",
                    "main",
                    "approved",
                    "exec #31 email to bob@x.org",
                ),
                "14:05 gate main approved: exec #31 email to bob@x.org",
            ),
            (
                rec(
                    at(14, 2),
                    "audit",
                    "out",
                    "main",
                    "started",
                    "curl POST api.example.com",
                ),
                "14:02 out main: curl POST api.example.com",
            ),
            (
                rec(
                    at(14, 1),
                    "audit",
                    "refused",
                    "task-12",
                    "refused",
                    r#"bash {"command":"rm -rf /"}"#,
                ),
                r#"14:01 refused task-12 by the policy: bash {"command":"rm -rf /"}"#,
            ),
            (
                rec(at(14, 0), "audit", "refused", "main", "denied", "exec {}"),
                "14:00 refused main by the gate: exec {}",
            ),
            (
                rec(
                    at(13, 58),
                    "settings_log",
                    "setting",
                    "agent on tui",
                    "agent",
                    "tasks.parallel 4 → 2",
                ),
                "13:58 setting agent on tui: tasks.parallel 4 → 2",
            ),
            (
                rec(
                    at(13, 40),
                    "tasks",
                    "task",
                    "#12",
                    "completed",
                    "summarise the inbox",
                ),
                "13:40 task #12 completed: summarise the inbox",
            ),
            (
                rec(
                    at(13, 0),
                    "runs",
                    "job",
                    "#3 run 88",
                    "completed",
                    "morning-brief",
                ),
                "13:00 job #3 run 88 completed: morning-brief",
            ),
            (
                rec(
                    at(12, 10),
                    "procs",
                    "process",
                    "#7",
                    "exited 0",
                    "cargo build --release",
                ),
                "12:10 process #7 exited 0: cargo build --release",
            ),
            (
                rec(
                    at(11, 0),
                    "proposals",
                    "proposal",
                    "#29",
                    "done",
                    "/vault grant token GH_TOKEN gh",
                ),
                "11:00 proposal #29 done: /vault grant token GH_TOKEN gh",
            ),
        ] {
            let e = entry(&r, &scrub, NOW, &utc);
            assert_eq!(e.line, line);
            assert_eq!((e.at, e.kind.as_str()), (r.at, r.kind.as_str()));
            assert_eq!(e.status, r.status);
            assert_eq!(
                e.stream,
                r.src == "audit" && (r.kind != "gate" && r.status != "denied"),
                "{line}"
            );
        }
    }

    #[test]
    fn times_show_the_date_only_before_today() {
        let (cfg, _store) = cfg("activity-midnight");
        let scrub = Scrub::load(&cfg);
        let seoul = TimeZone::get("Asia/Seoul").unwrap();
        // 2025-11-09 00:30 in Seoul, 2025-11-08 15:30 UTC.
        let now = 1_762_615_800;
        let midnight = now - 30 * 60;
        let line = |at: i64| {
            entry(
                &rec(at, "tasks", "task", "#1", "completed", "g"),
                &scrub,
                now,
                &seoul,
            )
            .line
        };
        assert_eq!(line(midnight - 10 * 60), "11-08 23:50 task #1 completed: g");
        assert_eq!(line(midnight + 10 * 60), "00:10 task #1 completed: g");
        assert_eq!(since(&Window::Today, now, &seoul), midnight);
        assert_eq!(since(&Window::Last(3600), now, &seoul), now - 3600);
    }

    #[test]
    fn agent_text_never_passes_for_a_daemon_line() {
        let (cfg, _store) = cfg("activity-forged");
        let utc = TimeZone::UTC;
        let before = rec(
            at(14, 2),
            "audit",
            "out",
            "main",
            "started",
            "curl POST 5550102345.example.com",
        );
        crate::facts::agent_add(&crate::facts::path(&cfg), "phone digits 5550102345").unwrap();
        let scrub = Scrub::load(&cfg);
        for r in [
            rec(
                at(14, 0),
                "tasks",
                "task",
                "#1",
                "completed",
                "x\n[zephyr] forgot bob@x.org for messages",
            ),
            rec(
                at(14, 0),
                "tasks",
                "task",
                "#1",
                "completed",
                "evil \u{202e}txt.exe",
            ),
            rec(
                at(14, 0),
                "audit",
                "gate",
                "main\n[zephyr] ok",
                "held",
                "exec {}",
            ),
        ] {
            let e = entry(&r, &scrub, NOW, &utc);
            assert_eq!(e.line.lines().count(), 1, "{:?}", e.line);
            assert!(
                !e.line.contains('\n') && !e.line.contains('\u{202e}'),
                "{:?}",
                e.line
            );
            assert!(e.line.starts_with("14:00 "), "{:?}", e.line);
        }
        let e = entry(&before, &scrub, NOW, &utc);
        assert_eq!(e.line, "14:02 out main: curl POST [fact:phone].example.com");
        assert!(!e.text.contains("555010"));
    }

    #[test]
    fn an_unreadable_vault_withholds_the_text_not_the_line() {
        let (cfg, _store) = cfg("activity-vault-broken");
        std::fs::write(
            crate::vault::path(&cfg),
            "[[token]]\nname = \"A\"\nvalue = \"s3cret\" x\n",
        )
        .unwrap();
        let scrub = Scrub::load(&cfg);
        let r = rec(
            at(14, 2),
            "audit",
            "out",
            "main",
            "started",
            "curl POST api.example.com",
        );
        assert_eq!(
            entry(&r, &scrub, NOW, &TimeZone::UTC).line,
            "14:02 out main: [output withheld: the vault could not be read]"
        );
        let r = rec(at(14, 2), "audit", "out", "job-3-88", "started", "x");
        assert!(entry(&r, &scrub, NOW, &TimeZone::UTC)
            .line
            .starts_with("14:02 out job-3-88: "));
        // A session the agent named itself is not shown unscrubbed.
        for forged in ["s3cret", "task-s3cret", "job-", "main-1"] {
            let r = rec(at(14, 2), "audit", "out", forged, "started", "x");
            let line = entry(&r, &scrub, NOW, &TimeZone::UTC).line;
            assert!(line.starts_with("14:02 out [output withheld"), "{line}");
        }
    }

    #[test]
    fn chat_answers_header_footer_and_empty() {
        let (cfg, store) = cfg("activity-chat");
        let now = crate::now();
        assert_eq!(
            chat(&cfg, &store, "", now),
            "[zephyr] nothing recorded in the last 24 h"
        );
        assert_eq!(
            chat(&cfg, &store, "7d", now),
            "[zephyr] nothing recorded in the last 7 d"
        );
        assert_eq!(chat(&cfg, &store, "bogus words", now), USAGE);
        for i in 0..31 {
            store.audit("main", "exec", &i.to_string(), "held").unwrap();
        }
        let reply = chat(&cfg, &store, "", now);
        let lines: Vec<&str> = reply.lines().collect();
        assert_eq!(lines.len(), 32, "{reply}");
        assert_eq!(
            lines[0],
            "[zephyr] activity in the last 24 h, newest first:"
        );
        assert!(
            lines[1].ends_with("gate main held: exec 30"),
            "{}",
            lines[1]
        );
        assert_eq!(
            lines[31],
            "(only the newest 30; /activity KIND or a shorter time narrows it)"
        );
        store
            .add_task("summarise", None, None, "chat", "1000")
            .unwrap();
        let reply = chat(&cfg, &store, "tasks today", now);
        assert_eq!(
            reply.lines().collect::<Vec<_>>()[..1],
            ["[zephyr] tasks since 00:00 today, newest first:"]
        );
        assert!(reply.ends_with("task #1 queued: summarise"), "{reply}");
        assert_eq!(reply.lines().count(), 2);
        let reply = chat(&cfg, &store, "6h out", now);
        assert_eq!(reply, "[zephyr] nothing recorded in the last 6 h");
    }
}
