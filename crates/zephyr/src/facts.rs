//! Private facts (approvals spec §4.5): things the owner never wants leaving the machine in an
//! outgoing call, such as a phone number or an address, kept in `<state>/facts.toml` (root's,
//! 0600, read fresh on each use). The gate looks for them in every call that sends something out.
//!
//! A fact is found however it is written: NFKC, case and spacing are ignored, a digits fact is
//! compared digit-only, and its percent-encoded and base64 forms count too. Values shorter than
//! the minimums (6 digits, 4 characters) never match, since they would be in everything.
//!
//! A value is never printed: `Debug` shows its length, and notices, audit rows and errors name
//! the fact only (`[fact:NAME]`).

use crate::config::Config;
use anyhow::{bail, Context};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table};
use unicode_normalization::UnicodeNormalization;

pub const FILE: &str = "facts.toml";

/// The shortest value that is matched: this many digits, or this many characters.
pub const MIN_DIGITS: usize = 6;
pub const MIN_CHARS: usize = 4;

/// `<state>/facts.toml`, beside vault.toml.
pub fn path(cfg: &Config) -> PathBuf {
    cfg.state_dir().join(FILE)
}

/// The facts file beside the state database at `db` (`<state>/state.db`), for the store, which
/// has no config, to scrub what it writes (R70). The store takes it from the path it was opened
/// at, not the file SQLite resolved, so a linked state.db still finds the right facts.
pub fn beside(db: &Path) -> PathBuf {
    db.with_file_name(FILE)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FactKind {
    #[default]
    Text,
    Digits,
    Email,
}

impl FactKind {
    pub fn parse(raw: &str) -> Option<FactKind> {
        match raw {
            "text" => Some(FactKind::Text),
            "digits" => Some(FactKind::Digits),
            "email" => Some(FactKind::Email),
            _ => None,
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            FactKind::Text => "text",
            FactKind::Digits => "digits",
            FactKind::Email => "email",
        }
    }
}

/// One private fact. `allow` lists targets (an address, a site) the owner lets it go to: a call
/// aimed at one of them is not checked for this fact.
#[derive(Clone, Default, PartialEq)]
pub struct Fact {
    pub name: String,
    pub kind: FactKind,
    pub value: String,
    pub allow: Vec<String>,
}

impl std::fmt::Debug for Fact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fact")
            .field("name", &self.name)
            .field("kind", &self.kind)
            .field("value", &self.value.len())
            .field("allow", &self.allow)
            .finish()
    }
}

impl Fact {
    pub fn check(&self) -> anyhow::Result<()> {
        let name = &self.name;
        if !crate::vault::login_name(name) {
            bail!(
                "{name:?} is not a fact name: lowercase letters, digits, '-' and '_', at most 64"
            );
        }
        if self.value.chars().any(char::is_control) || self.value.chars().count() > 500 {
            bail!("fact {name}: the value is one line of at most 500 characters");
        }
        match self.kind {
            FactKind::Digits if digits_of(&self.value).len() < MIN_DIGITS => {
                bail!("fact {name}: a digits fact needs at least {MIN_DIGITS} digits")
            }
            FactKind::Text | FactKind::Email if norm(&self.value).chars().count() < MIN_CHARS => {
                bail!("fact {name}: a value needs at least {MIN_CHARS} characters")
            }
            FactKind::Email if !self.value.contains('@') => {
                bail!("fact {name}: an email fact needs an address")
            }
            _ => {}
        }
        for target in &self.allow {
            if target.trim().is_empty()
                || target.chars().count() > 200
                || target.chars().any(char::is_control)
            {
                bail!("fact {name}: an allowed target is one short line");
            }
        }
        Ok(())
    }

    /// What anyone may see: no value.
    pub fn public(&self) -> Value {
        json!({"name": self.name, "kind": self.kind.key(), "allow": self.allow})
    }

    /// One line for `/facts`, `zephyr facts list` and the TUI.
    pub fn line(&self) -> String {
        let allow = match self.allow.as_slice() {
            [] => "none".to_string(),
            a => a.join(", "),
        };
        format!("{} ({}) allow: {allow}", self.name, self.kind.key())
    }
}

/// NFKC, lower case, runs of whitespace as one space, trimmed.
fn norm(s: &str) -> String {
    let folded: String = s.nfkc().collect::<String>().to_lowercase();
    folded.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The ASCII digits of `s` after NFKC (full-width digits count).
fn digits_of(s: &str) -> String {
    s.nfkc().filter(char::is_ascii_digit).collect()
}

/// One character of a text, with the byte range of the original it stands for.
type Piece = (char, usize, usize);

/// The characters of `text` as they are.
fn raw(text: &str) -> Vec<Piece> {
    text.char_indices()
        .map(|(i, c)| (c, i, i + c.len_utf8()))
        .collect()
}

/// `text` percent-decoded, with `+` as a space (a form body); each decoded character keeps the
/// range of the `%XX` bytes it came from. `None` when there is nothing to decode.
fn decoded(text: &str) -> Option<Vec<Piece>> {
    let b = text.as_bytes();
    let hex = |i: usize| -> Option<u8> {
        let h = std::str::from_utf8(b.get(i..i + 2)?).ok()?;
        u8::from_str_radix(h, 16).ok()
    };
    let (mut bytes, mut at, mut i) = (Vec::new(), Vec::new(), 0);
    while i < b.len() {
        match b[i] {
            b'%' if hex(i + 1).is_some() => {
                bytes.push(hex(i + 1).unwrap_or(0));
                at.push((i, i + 3));
                i += 3;
            }
            b'+' => {
                bytes.push(b' ');
                at.push((i, i + 1));
                i += 1;
            }
            c => {
                bytes.push(c);
                at.push((i, i + 1));
                i += 1;
            }
        }
    }
    if bytes == b {
        return None;
    }
    let (mut out, mut pos) = (Vec::new(), 0);
    for chunk in bytes.utf8_chunks() {
        for c in chunk.valid().chars() {
            let n = c.len_utf8();
            out.push((c, at[pos].0, at[pos + n - 1].1));
            pos += n;
        }
        for _ in chunk.invalid() {
            out.push(('\u{fffd}', at[pos].0, at[pos].1));
            pos += 1;
        }
    }
    Some(out)
}

/// Whether `c` shows nothing and so cannot keep a value from being one (R40): a control that is
/// not whitespace, a zero-width or direction mark (U+061C too), or a byte-order mark. A line break and a tab
/// are spacing, not invisible.
pub(crate) fn invisible(c: char) -> bool {
    (c.is_control() && !c.is_whitespace())
        || matches!(
            c,
            '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{2069}'
                | '\u{feff}'
                | '\u{061c}'
        )
}

/// `view` without its invisible characters.
fn visible(mut view: Vec<Piece>) -> Vec<Piece> {
    view.retain(|p| !invisible(p.0));
    view
}

/// `view` after NFKC, lower case and collapsing runs of whitespace to one space; a character
/// keeps the range it came from, so a match maps back to the text it was found in.
fn folded(view: &[Piece]) -> Vec<Piece> {
    let mut out: Vec<Piece> = Vec::new();
    for &(c, from, to) in view {
        for n in c.to_string().nfkc().flat_map(char::to_lowercase) {
            match (n.is_whitespace(), out.last_mut()) {
                (true, Some(last)) if last.0 == ' ' => last.2 = to,
                (true, _) => out.push((' ', from, to)),
                (false, _) => out.push((n, from, to)),
            }
        }
    }
    out
}

/// Whether `c` (after NFKC) can sit between the digits of one written number: spaces, tabs,
/// slashes, dots, commas, brackets and every kind of dash.
fn separator(c: char) -> bool {
    c.to_string().nfkc().all(|n| {
        n.is_whitespace()
            || matches!(
                n,
                '-' | '.' | ',' | '(' | ')' | '/' | '\u{2010}'..='\u{2015}' | '\u{2212}'
            )
    })
}

/// `view` with each spoken digit read as that digit, so a number said in words is a run too: an
/// English or French digit word (`zero`/`oh` to `nine`, `zéro` to `neuf`) standing as a whole
/// word, and each Sino-Korean digit syllable (`영`/`공`, `일` to `구`), which Korean writes with
/// no space between. The digit keeps the range of the word it stands for.
fn spoken(view: &[Piece]) -> Vec<Piece> {
    const WORDS: &[(&str, char)] = &[
        ("zero", '0'),
        ("oh", '0'),
        ("one", '1'),
        ("two", '2'),
        ("three", '3'),
        ("four", '4'),
        ("five", '5'),
        ("six", '6'),
        ("seven", '7'),
        ("eight", '8'),
        ("nine", '9'),
        ("zéro", '0'),
        ("un", '1'),
        ("deux", '2'),
        ("trois", '3'),
        ("quatre", '4'),
        ("cinq", '5'),
        ("sept", '7'),
        ("huit", '8'),
        ("neuf", '9'),
    ];
    const KOREAN: [(char, char); 11] = [
        ('영', '0'),
        ('공', '0'),
        ('일', '1'),
        ('이', '2'),
        ('삼', '3'),
        ('사', '4'),
        ('오', '5'),
        ('육', '6'),
        ('칠', '7'),
        ('팔', '8'),
        ('구', '9'),
    ];
    // A combining mark belongs to its word, so a decomposed `zéro` is one word.
    let latin = |c: char| {
        (c.is_alphabetic() || unicode_normalization::char::is_combining_mark(c))
            && !('\u{ac00}'..='\u{d7a3}').contains(&c)
    };
    let mut out = Vec::new();
    let mut i = 0;
    while i < view.len() {
        let (c, from, to) = view[i];
        if !latin(c) {
            let digit = KOREAN.iter().find(|(k, _)| *k == c).map(|(_, d)| *d);
            out.push((digit.unwrap_or(c), from, to));
            i += 1;
            continue;
        }
        let end = view[i..]
            .iter()
            .position(|p| !latin(p.0))
            .map_or(view.len(), |n| i + n);
        // No digit word is over 6 letters (with a decomposed accent, 7 characters): a longer
        // word is never built and normalized.
        let letters = &view[i..end];
        let digit = match letters.len() {
            2..=7 if letters.iter().all(|p| p.0.is_ascii()) => WORDS.iter().find(|(w, _)| {
                w.len() == letters.len()
                    && w.bytes()
                        .zip(letters)
                        .all(|(b, p)| p.0.eq_ignore_ascii_case(&char::from(b)))
            }),
            2..=7 => {
                let w: String = letters.iter().flat_map(|p| p.0.to_lowercase()).collect();
                let w: String = w.nfc().collect();
                WORDS.iter().find(|(word, _)| *word == w)
            }
            _ => None,
        };
        match digit {
            Some((_, d)) => out.push((*d, from, view[end - 1].2)),
            None => out.extend_from_slice(&view[i..end]),
        }
        i = end;
    }
    out
}

/// One run of digits: each digit's byte range, the digits, and whether a `+` is right before it.
struct Run {
    at: Vec<(usize, usize)>,
    digits: String,
    plus: bool,
}

/// The digit runs in `view`: a run is digits with the separators phone numbers and card
/// numbers are written with between them.
fn runs(view: &[Piece]) -> Vec<Run> {
    let digit = |c: char| -> Option<char> {
        let mut it = c.to_string().nfkc().collect::<Vec<_>>().into_iter();
        match (it.next(), it.next()) {
            (Some(d), None) if d.is_ascii_digit() => Some(d),
            _ => None,
        }
    };
    let mut out = Vec::new();
    let mut cur: Option<Run> = None;
    let mut before = None;
    for &(c, from, to) in view {
        match (digit(c), &mut cur) {
            (Some(d), Some(run)) => {
                run.at.push((from, to));
                run.digits.push(d);
            }
            (Some(d), None) => {
                cur = Some(Run {
                    at: vec![(from, to)],
                    digits: d.to_string(),
                    plus: before == Some('+'),
                })
            }
            (None, Some(_)) if separator(c) => {}
            (None, _) => out.extend(cur.take()),
        }
        before = Some(c);
    }
    out.extend(cur);
    out
}

/// `digits` without the zeros a national number starts with (the trunk prefix), when at least
/// the minimum is left.
fn untrunked(digits: &str) -> &str {
    let t = digits.trim_start_matches('0');
    if t.len() >= MIN_DIGITS {
        t
    } else {
        digits
    }
}

/// Where in `run` (a range of its digits) the digits `value` are, written another way: the value
/// inside the run, the run the value without a country code (up to three digits), or the value
/// without its country code inside the run, trunk zeros aside (`+82 10…` is `010…`). The range
/// takes in the trunk zeros before it, and the country code of a run written `+CC…`.
fn same_number(value: &str, run: &Run) -> Option<(usize, usize)> {
    let (v, r) = (untrunked(value), untrunked(&run.digits));
    let zeros = run.digits.len() - r.len();
    if r.len() >= MIN_DIGITS && v.ends_with(r) && v.len() <= r.len() + 3 {
        return Some((0, run.digits.len()));
    }
    let (at, len) = std::iter::once(v)
        .chain(
            (1..=3)
                .filter(|k| v.len() >= k + MIN_DIGITS)
                .map(|k| &v[k..]),
        )
        .find_map(|v| r.find(v).map(|at| (zeros + at, v.len())))?;
    let digits = run.digits.as_bytes();
    let mut from = at;
    while from > 0 && digits[from - 1] == b'0' {
        from -= 1;
    }
    if run.plus && from <= 3 {
        from = 0;
    }
    Some((from, at + len))
}

/// Whether the fact's value is long enough to be looked for at all.
fn matchable(f: &Fact) -> bool {
    match f.kind {
        FactKind::Digits => digits_of(&f.value).len() >= MIN_DIGITS,
        _ => norm(&f.value).chars().count() >= MIN_CHARS,
    }
}

/// Whether `target` is one the owner allowed `f` to go to.
fn allowed(f: &Fact, target: &str) -> bool {
    let target = norm(target);
    f.allow.iter().any(|a| norm(a) == target)
}

/// Where in `text` (byte ranges) `f` is, in any of its forms.
fn spans(f: &Fact, text: &str) -> Vec<(usize, usize)> {
    if !matchable(f) {
        return Vec::new();
    }
    let mut out = Vec::new();
    // The value as written, in the text's own terms: case, spacing, Unicode forms and invisible
    // characters ignored.
    let pattern: Vec<char> = {
        let p: Vec<char> = folded(&visible(raw(&f.value)))
            .into_iter()
            .map(|(c, ..)| c)
            .collect();
        let (from, to) = (
            p.iter().position(|c| *c != ' ').unwrap_or(p.len()),
            p.iter().rposition(|c| *c != ' ').map_or(0, |i| i + 1),
        );
        p[from..to.max(from)].to_vec()
    };
    let value = digits_of(&f.value);
    // The text as it is, and percent-decoded (a URL, a form body), its invisible characters
    // left out: a match still spans them, so scrubbing removes them with the value (R40).
    let views = std::iter::once(raw(text)).chain(decoded(text)).map(visible);
    for view in views {
        if f.kind == FactKind::Digits {
            for run in runs(&spoken(&view)) {
                if let Some((from, to)) = same_number(&value, &run) {
                    out.push((run.at[from].0, run.at[to - 1].1));
                }
            }
        }
        let hay = folded(&view);
        if !pattern.is_empty() && hay.len() >= pattern.len() {
            for at in 0..=hay.len() - pattern.len() {
                if hay[at..at + pattern.len()]
                    .iter()
                    .zip(&pattern)
                    .all(|(h, p)| h.0 == *p)
                {
                    out.push((hay[at].1, hay[at + pattern.len() - 1].2));
                }
            }
        }
    }
    // A5: base64, at any alignment; a digits fact's digit-only value too.
    let mut forms = crate::vault::base64_forms(&f.value);
    if f.kind == FactKind::Digits && value != f.value {
        forms.extend(crate::vault::base64_forms(&value));
    }
    for form in forms {
        out.extend(text.match_indices(&form).map(|(i, m)| (i, i + m.len())));
    }
    out
}

/// D6: whether `f` is in `text` with NFKC applied to the whole of each, not character by
/// character as `spans` does, so a value written decomposed (NFD Hangul, as macOS writes names)
/// matches it composed, and the other way round.
fn whole(f: &Fact, text: &str) -> bool {
    let strip = |s: &str| -> String { s.chars().filter(|c| !invisible(*c)).collect() };
    let value = norm(&strip(&f.value));
    matchable(f) && !value.is_empty() && norm(&strip(text)).contains(&value)
}

/// The names of the facts in `text`, once each. A fact whose `allow` names `target` is skipped.
pub fn found<'a>(facts: &'a [Fact], text: &str, target: Option<&str>) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    for f in facts {
        let skip = target.is_some_and(|t| allowed(f, t));
        let here = || !spans(f, text).is_empty() || whole(f, text);
        if !skip && !names.contains(&f.name.as_str()) && here() {
            names.push(&f.name);
        }
    }
    names
}

/// `text` with each fact found in it replaced by `[fact:NAME]`, for audit rows and notices. A
/// fact overlapping another one's span is covered with it: no part of either is left.
pub fn scrub(facts: &[Fact], text: &str) -> String {
    let label = |name: &str| format!("[fact:{name}]");
    let withheld = |name: &str| format!("[withheld: it holds the private fact {name}]");
    scrub_as(facts, text, &label, &withheld)
}

/// [`scrub`] with each value's replacement written by `label`, and by `withheld` when it is
/// found where it cannot be placed: how a live card number is hidden too (`cards::scrub`).
pub(crate) fn scrub_as(
    facts: &[Fact],
    text: &str,
    label: &dyn Fn(&str) -> String,
    withheld: &dyn Fn(&str) -> String,
) -> String {
    let mut marks: Vec<(usize, usize, &str)> = Vec::new();
    for f in facts {
        let at = spans(f, text);
        // D6: found where it cannot be placed: none of the text is shown.
        if at.is_empty() && whole(f, text) {
            return withheld(&f.name);
        }
        marks.extend(at.into_iter().map(|(a, b)| (a, b, f.name.as_str())));
    }
    // Leftmost first, the longest of those starting together.
    marks.sort_by_key(|&(a, b, _)| (a, std::cmp::Reverse(b)));
    let (mut out, mut at) = (String::new(), 0);
    for (a, b, name) in marks {
        if a < at {
            at = at.max(b);
            continue;
        }
        out.push_str(&text[at..a]);
        out.push_str(&label(name));
        at = b;
    }
    out.push_str(&text[at..]);
    out
}

/// A `gateway` command as the audit and the log may keep it, which must never hold a value:
/// read trimmed, as the plugin and the daemon read it (B1). `/facts add NAME KIND VALUE`, with
/// any whitespace between its words, keeps its name and kind only when they are a valid name
/// and a valid kind (and the name holds neither the value nor a number), else it is
/// `/facts add [value]`; a
/// well-formed `/facts`, `/facts rm` or `/facts allow` is kept; any other `/facts…` is only
/// `/facts …` (A4). Any other command is as it came.
pub fn redact(command: &str) -> String {
    let c = command.trim();
    let Some(rest) = c.strip_prefix("/facts") else {
        return command.into();
    };
    let words: Vec<&str> = rest.split_whitespace().collect();
    let spaced = rest.is_empty() || rest.starts_with(char::is_whitespace);
    if spaced && words.first() == Some(&"add") {
        let value = words.get(3..).map(|v| v.join(" ")).unwrap_or_default();
        let kept = match (words.get(1), words.get(2).and_then(|k| FactKind::parse(k))) {
            // a name with a number's worth of digits may be the value itself
            (Some(name), Some(kind))
                if crate::vault::login_name(name) && digits_of(name).len() < MIN_DIGITS =>
            {
                let f = Fact {
                    name: name.to_string(),
                    kind,
                    value,
                    allow: vec![],
                };
                spans(&f, name)
                    .is_empty()
                    .then(|| format!("{name} {}", kind.key()))
            }
            _ => None,
        };
        return match kept {
            Some(kept) => format!("/facts add {kept} [value]"),
            None => "/facts add [value]".into(),
        };
    }
    match spaced && zephyr_policy::tools::gateway_kind(c).is_some() {
        true => c.into(),
        false => "/facts …".into(),
    }
}

/// `text` with the facts as they are now scrubbed out. Fails closed, as `vault::scrubbed` does.
pub fn scrubbed(cfg: &Config, text: String) -> String {
    scrubbed_in(&path(cfg), text)
}

/// `text` scrubbed of the facts in the file at `facts`; fails closed.
pub fn scrubbed_in(facts: &Path, text: String) -> String {
    match load(facts) {
        Ok(facts) => scrub(&facts, &text),
        Err(_) => "[output withheld: the private facts could not be read]".into(),
    }
}

/// Names and kinds, no value and no target: what the agent may read.
pub fn names(facts: &[Fact]) -> String {
    match facts.is_empty() {
        true => "no private facts saved".into(),
        false => {
            let lines: Vec<String> = facts
                .iter()
                .map(|f| format!("{} ({})", f.name, f.kind.key()))
                .collect();
            format!("private facts:\n{}", lines.join("\n"))
        }
    }
}

// ---- the file ----

impl Fact {
    fn read(t: &Table) -> Fact {
        let s = |k: &str| t.get(k).and_then(Item::as_str).unwrap_or("").to_string();
        Fact {
            name: s("name"),
            kind: FactKind::parse(&s("kind")).unwrap_or_default(),
            value: s("value"),
            allow: (t.get("allow").and_then(Item::as_array))
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default(),
        }
    }
}

/// The facts in `text` (a `[[fact]]` table each). The error names a line, never a value.
pub fn parse(text: &str) -> anyhow::Result<Vec<Fact>> {
    let doc = document(text)?;
    let tables = doc.get("fact").and_then(Item::as_array_of_tables);
    let mut facts: Vec<Fact> = Vec::new();
    for t in tables.into_iter().flatten() {
        let kind = t.get("kind").and_then(Item::as_str).unwrap_or("text");
        if FactKind::parse(kind).is_none() {
            bail!("facts.toml: a fact has the kind {kind:?}; text, digits or email");
        }
        let f = Fact::read(t);
        f.check().context("facts.toml")?;
        if facts.iter().any(|o| o.name == f.name) {
            bail!("facts.toml has two facts named {}", f.name);
        }
        facts.push(f);
    }
    Ok(facts)
}

fn document(text: &str) -> anyhow::Result<DocumentMut> {
    // Only the line: the parser's own message quotes the text around the error, a value maybe.
    let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
        let at = e.span().map_or(0, |s| s.start).min(text.len());
        let line = text.as_bytes()[..at]
            .iter()
            .filter(|&&b| b == b'\n')
            .count()
            + 1;
        anyhow::anyhow!("facts.toml is not valid TOML (line {line})")
    })?;
    if doc
        .get("fact")
        .is_some_and(|i| i.as_array_of_tables().is_none())
    {
        bail!("facts.toml: `fact` must be [[fact]] tables");
    }
    Ok(doc)
}

/// `text` with its `[[fact]]` tables replaced by `facts`; anything else in it stays.
pub fn render(text: &str, facts: &[Fact]) -> anyhow::Result<String> {
    let mut doc = document(text)?;
    let mut tables = ArrayOfTables::new();
    for f in facts {
        let mut t = Table::new();
        t["name"] = toml_edit::value(f.name.as_str());
        t["kind"] = toml_edit::value(f.kind.key());
        t["value"] = toml_edit::value(f.value.as_str());
        t["allow"] = toml_edit::value(Array::from_iter(f.allow.iter().map(String::as_str)));
        tables.push(t);
    }
    match facts.is_empty() {
        true => {
            doc.remove("fact");
        }
        false => doc["fact"] = Item::ArrayOfTables(tables),
    }
    Ok(doc.to_string())
}

/// An empty list when there is no file yet.
pub fn load(path: &Path) -> anyhow::Result<Vec<Fact>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        r => parse(&r.with_context(|| format!("reading {}", path.display()))?),
    }
}

/// One writer at a time in this process: the router's commands and the attach port's saves.
static EDIT: Mutex<()> = Mutex::new(());

/// Loads the facts, applies `f` and saves them, as one step.
pub fn edit<T>(
    path: &Path,
    f: impl FnOnce(&mut Vec<Fact>) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let _held = EDIT.lock().unwrap_or_else(|e| e.into_inner());
    let text = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        r => r.with_context(|| format!("reading {}", path.display()))?,
    };
    let mut facts = parse(&text)?;
    let out = f(&mut facts)?;
    for fact in &facts {
        fact.check()?;
    }
    crate::host::conf::replace_private(path, &render(&text, &facts)?)?;
    Ok(out)
}

/// Adds `fact`, or replaces the one of its name (`replace`). `true` when it is new.
pub fn put(facts: &mut Vec<Fact>, fact: Fact, replace: bool) -> anyhow::Result<bool> {
    fact.check()?;
    match facts.iter_mut().find(|f| f.name == fact.name) {
        Some(_) if !replace => bail!("a fact named {} already exists", fact.name),
        Some(old) => {
            *old = fact;
            Ok(false)
        }
        None => {
            facts.push(fact);
            Ok(true)
        }
    }
}

/// Removes the fact `name`; `false` when there is none.
pub fn remove(facts: &mut Vec<Fact>, name: &str) -> bool {
    let before = facts.len();
    facts.retain(|f| f.name != name);
    facts.len() != before
}

/// Lets fact `name` go to `target` (`allow`), or stops that.
pub fn allow(facts: &mut [Fact], name: &str, target: &str, allow: bool) -> anyhow::Result<String> {
    let f = (facts.iter_mut().find(|f| f.name == name))
        .with_context(|| format!("no fact named {name}"))?;
    let target = target.trim();
    let listed = f.allow.iter().position(|a| norm(a) == norm(target));
    match (allow, listed) {
        (true, Some(_)) => bail!("{name} already goes to {target}"),
        (true, None) => {
            f.allow.push(target.to_string());
            f.check()?;
            Ok(format!("{name} may go to {target} without a check"))
        }
        (false, Some(i)) => {
            f.allow.remove(i);
            Ok(format!("{name} is checked again for {target}"))
        }
        (false, None) => bail!("{name} was not allowed for {target}"),
    }
}

/// Names, kinds and allowed targets; never a value.
pub fn list(facts: &[Fact]) -> String {
    match facts.is_empty() {
        true => "[zephyr] no private facts saved".into(),
        false => {
            let lines: Vec<String> = facts.iter().map(Fact::line).collect();
            format!("[zephyr] private facts:\n{}", lines.join("\n"))
        }
    }
}

/// The owner's `/facts …`, on any channel. A value is never taken in a chat message. Letting a
/// fact go somewhere, and removing one, loosen the check, so they need a strong channel
/// (`strong`); an approved proposal arrives here as the owner's own command.
pub fn command(path: &Path, arg: &str, strong: bool) -> Result<String, String> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let edit = |f: &dyn Fn(&mut Vec<Fact>) -> anyhow::Result<String>| match edit(path, f) {
        Ok(text) => Ok(format!("[zephyr] {text}")),
        Err(e) => Err(format!("[zephyr] {e:#}")),
    };
    let weak = "[zephyr] loosening the check for a private fact needs a strong channel";
    match words[..] {
        [] | ["list"] => (load(path).map(|f| list(&f))).map_err(|e| format!("[zephyr] {e:#}")),
        ["rm" | "allow", ..] if !strong => Err(weak.into()),
        ["rm", name] => edit(&|f| match remove(f, name) {
            true => Ok(format!("removed the fact {name}")),
            false => bail!("no fact named {name}"),
        }),
        ["allow", name, target] => edit(&|f| allow(f, name, target, true)),
        ["disallow", name, target] => edit(&|f| allow(f, name, target, false)),
        ["add" | "edit", ..] => Err(
            "[zephyr] values never go through a chat: in zephyr attach, \
            /facts opens a form; on the host, run `zephyr facts add NAME --kind text|digits|email`"
                .into(),
        ),
        _ => Err(
            "[zephyr] usage: /facts, /facts rm NAME, /facts allow NAME TARGET, \
              /facts disallow NAME TARGET"
                .into(),
        ),
    }
}

/// The agent's `/facts add NAME KIND VALUE` (`arg` after `add`): saved at once, since a new fact
/// only makes calls stricter. It never replaces a fact. The answer names the fact, not the value.
pub fn agent_add(path: &Path, arg: &str) -> Result<String, String> {
    let mut it = arg.splitn(3, ' ');
    let (Some(name), Some(kind), Some(value)) = (it.next(), it.next(), it.next()) else {
        return Err("usage: /facts add NAME KIND VALUE (KIND: text, digits or email)".into());
    };
    let Some(kind) = FactKind::parse(kind) else {
        return Err("the kind is text, digits or email".into());
    };
    let fact = Fact {
        name: name.into(),
        kind,
        value: value.trim().into(),
        allow: vec![],
    };
    edit(path, |f| put(f, fact, false))
        .map(|_| format!("saved the private fact {name}"))
        .map_err(|e| format!("{e:#}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fact(name: &str, kind: FactKind, value: &str) -> Fact {
        Fact {
            name: name.into(),
            kind,
            value: value.into(),
            allow: vec![],
        }
    }

    /// The store finds the facts beside its database: the same file the config names.
    #[test]
    fn the_facts_lie_beside_the_state_database() {
        for extra in ["", "state = '/var/lib/zephyr'\n"] {
            let text = format!("rpi = '/r'\nhome = '/zephyr'\ntimezone = 'UTC'\n{extra}");
            let cfg: Config = toml::from_str(&text).unwrap();
            assert_eq!(beside(&cfg.db_path()), path(&cfg));
        }
    }

    #[test]
    fn digits_match_across_formatting() {
        let phone = [fact("phone", FactKind::Digits, "5550102345")];
        for text in [
            "call +1 (555) 010-2345 now",
            "5550102345",
            "tel: 555.010.2345",
            "555 010 2345",
            "５５５０１０２３４５",
        ] {
            assert_eq!(found(&phone, text, None), ["phone"], "{text}");
        }
        // The value written with a country code, the text without it.
        let intl = [fact("phone", FactKind::Digits, "+1 (555) 010-2345")];
        assert_eq!(found(&intl, "5550102345", None), ["phone"]);
        assert_eq!(found(&intl, "+15550102345", None), ["phone"]);
        // Other numbers are left alone.
        assert!(found(&phone, "order 5550102346", None).is_empty());
        assert!(found(&phone, "555 0102", None).is_empty());
    }

    #[test]
    fn digits_said_as_words_match() {
        let phone = [fact("phone", FactKind::Digits, "046454286")];
        for text in [
            "My number is zero four six four five four two eight six.",
            "oh four six, four five four, two eight six",
            "Zero-Four-Six 454 two eight six",
            "zéro quatre six quatre cinq quatre deux huit six",
            "공사육 사오사 이팔육이에요",
            "영사육사오사이팔육",
            "046, 454, 286",
        ] {
            assert_eq!(found(&phone, text, None), ["phone"], "{text}");
        }
        assert_eq!(
            scrub(&phone, "it is oh four six four five four two eight six, ok"),
            "it is [fact:phone], ok"
        );
        // A word only counts whole, and other numbers stay other numbers.
        for text in [
            "zero four six four five four two eight seven",
            "zero four six four five four two eight sixty",
            "zero four six four five four two eighty six",
        ] {
            assert!(found(&phone, text, None).is_empty(), "{text}");
        }
    }

    /// PR #70 review: a digit beside the number (a spoken one, one after a comma) makes the run
    /// longer; the number in it is still found, and only the number is scrubbed.
    #[test]
    fn a_number_inside_a_longer_run_is_found_and_scrubbed_alone() {
        let ca = [fact("phone", FactKind::Digits, "+1 416 555 0142")];
        for (text, scrubbed) in [
            (
                "Table for two. 416 555 0142.",
                "Table for two. [fact:phone].",
            ),
            ("Table for 2. 416 555 0142.", "Table for 2. [fact:phone]."),
            ("Call 416-555-0142, 2 people", "Call [fact:phone], 2 people"),
            ("Ann,416-555-0142,3", "Ann,[fact:phone],3"),
            ("416 555 0142 one more thing", "[fact:phone] one more thing"),
            (
                "mon numéro: 416 555 0142 un instant",
                "mon numéro: [fact:phone] un instant",
            ),
            // The country code said too is the whole value.
            ("one, 416 555 0142", "[fact:phone]"),
            ("call +1 (416) 555-0142 now", "call +[fact:phone] now"),
        ] {
            assert_eq!(found(&ca, text, None), ["phone"], "{text}");
            assert_eq!(scrub(&ca, text), scrubbed, "{text}");
        }
        let kr = [fact("phone", FactKind::Digits, "+82 10-1234-5678")];
        for (text, scrubbed) in [
            (
                "제 번호는 010-1234-5678이에요",
                "제 번호는 [fact:phone]이에요",
            ),
            ("이 010-1234-5678 번호로", "이 [fact:phone] 번호로"),
            ("010-1234-5678입니다", "[fact:phone]입니다"),
        ] {
            assert_eq!(found(&kr, text, None), ["phone"], "{text}");
            assert_eq!(scrub(&kr, text), scrubbed, "{text}");
        }
        // A decomposed accent is the same word.
        let phone = [fact("phone", FactKind::Digits, "4165550142")];
        let nfd = "quatre un six cinq cinq cinq ze\u{301}ro un quatre deux";
        assert_eq!(found(&phone, nfd, None), ["phone"]);
    }

    #[test]
    fn short_values_never_match() {
        let facts = [
            fact("pin", FactKind::Digits, "12345"),
            fact("nick", FactKind::Text, "abc"),
            fact("mail", FactKind::Email, "a@b"),
        ];
        assert!(found(&facts, "12345 abc a@b and 123456", None).is_empty());
        assert_eq!(scrub(&facts, "12345 abc"), "12345 abc");
        assert!(facts.iter().all(|f| f.check().is_err()));
    }

    #[test]
    fn encoded_forms_match() {
        let facts = [
            fact("addr", FactKind::Text, "12 Elm Street, Springfield"),
            fact("mail", FactKind::Email, "Owner@Example.com"),
            fact("token", FactKind::Text, "correct-horse-battery"),
        ];
        let url = "https://x.test/?q=12%20Elm%20Street%2C%20Springfield&a=owner%40example.com";
        assert_eq!(found(&facts, url, None), ["addr", "mail"]);
        let form = "q=12+elm+street%2c+springfield";
        assert_eq!(found(&facts, form, None), ["addr"]);
        // Case and spacing do not matter.
        assert_eq!(found(&facts, "OWNER@example.COM", None), ["mail"]);
        assert_eq!(
            found(&facts, "12  elm\nstreet, springfield", None),
            ["addr"]
        );
        // base64, at any alignment.
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD.encode("user:correct-horse-battery");
        assert_eq!(
            found(&facts, &format!("Authorization: Basic {b}"), None),
            ["token"]
        );
        let scrubbed = scrub(&facts, &format!("{url} {b}"));
        assert!(scrubbed.contains("[fact:addr]") && scrubbed.contains("[fact:mail]"));
        assert!(scrubbed.contains("[fact:token]"), "{scrubbed}");
        assert!(
            !scrubbed.contains("Elm") && !scrubbed.contains("40example"),
            "{scrubbed}"
        );
    }

    #[test]
    fn an_allowed_target_skips_the_check() {
        let mut mail = fact("mail", FactKind::Email, "owner@example.com");
        mail.allow = vec!["Me@Example.com".into()];
        let facts = [mail];
        let text = "my address is owner@example.com";
        assert_eq!(found(&facts, text, None), ["mail"]);
        assert_eq!(found(&facts, text, Some("someone@else.org")), ["mail"]);
        assert!(found(&facts, text, Some("me@example.com")).is_empty());
    }

    #[test]
    fn trunk_prefixes_and_country_codes_match_both_ways() {
        let local = [fact("phone", FactKind::Digits, "010-1234-5678")];
        let intl = [fact("phone", FactKind::Digits, "+82 10-1234-5678")];
        assert_eq!(found(&local, "call +82 10-1234-5678", None), ["phone"]);
        assert_eq!(found(&local, "call +82 (0)10 1234 5678", None), ["phone"]);
        assert_eq!(found(&intl, "call 010-1234-5678", None), ["phone"]);
        let uk_local = [fact("uk", FactKind::Digits, "020 7946 0958")];
        let uk_intl = [fact("uk", FactKind::Digits, "+44 20 7946 0958")];
        assert_eq!(found(&uk_local, "+44 20 7946 0958", None), ["uk"]);
        assert_eq!(found(&uk_intl, "020 7946 0958", None), ["uk"]);
        assert!(found(&local, "010-1234-5679", None).is_empty());
        assert!(found(&uk_intl, "7946 0958", None).is_empty());
    }

    #[test]
    fn dashes_and_full_width_separators_match() {
        let phone = [fact("phone", FactKind::Digits, "5550102345")];
        for text in [
            "５５５－０１０－２３４５",
            "555\u{2013}010\u{2013}2345",
            "555\u{2010}010\u{2011}2345",
            "555\u{2212}010\u{2212}2345",
            "555\t010\t2345",
            "555\u{3000}010\u{3000}2345",
        ] {
            assert_eq!(found(&phone, text, None), ["phone"], "{text:?}");
        }
        assert_eq!(
            scrub(&phone, "tel ５５５－０１０－２３４５ ok"),
            "tel [fact:phone] ok"
        );
    }

    #[test]
    fn partly_encoded_text_and_form_bodies_match() {
        let facts = [
            fact("addr", FactKind::Text, "12 Elm Street, Springfield"),
            fact("phone", FactKind::Digits, "5550102345"),
        ];
        for text in [
            "q=12%20Elm%20Street,%20Springfield",
            "q=12+Elm+Street%2C+Springfield&x=1",
            "q=12%20elm%20street%2c%20springfield",
        ] {
            assert_eq!(found(&facts, text, None), ["addr"], "{text}");
        }
        for text in [
            "phone=555%20010%202345",
            "phone=555+010+2345",
            "p=555%2D010%2D2345",
        ] {
            assert_eq!(found(&facts, text, None), ["phone"], "{text}");
        }
        // The original bytes are what is replaced.
        assert_eq!(
            scrub(&facts, "a=1&phone=555+010+2345&b=2"),
            "a=1&phone=[fact:phone]&b=2"
        );
        assert_eq!(
            scrub(&facts, "q=12%20Elm%20Street,%20Springfield!"),
            "q=[fact:addr]!"
        );
    }

    #[test]
    fn overlapping_facts_leave_no_part_of_either() {
        let facts = [
            fact("a", FactKind::Text, "12 Elm Street"),
            fact("b", FactKind::Text, "Elm Street, Springfield"),
        ];
        let out = scrub(&facts, "x 12 Elm Street, Springfield y");
        assert!(
            !out.contains("Elm") && !out.contains("Springfield"),
            "{out}"
        );
        assert!(out.starts_with("x [fact:") && out.ends_with(" y"), "{out}");
    }

    #[test]
    fn a_facts_add_is_redacted_to_name_and_kind() {
        for (cmd, want) in [
            (
                "/facts add phone digits +1 555 010 2345",
                "/facts add phone digits [value]",
            ),
            ("/facts add phone", "/facts add [value]"),
            ("/facts add", "/facts add [value]"),
            ("/facts rm phone", "/facts rm phone"),
            (
                "/facts allow mail mum@example.com",
                "/facts allow mail mum@example.com",
            ),
            ("/facts", "/facts"),
            ("/status", "/status"),
        ] {
            assert_eq!(redact(cmd), want, "{cmd}");
        }
    }

    /// A4, B1: however the command is spaced, a value never survives: the command is read
    /// trimmed, `/facts` and `add` with any whitespace between; a word is kept only as a valid
    /// fact name followed by a valid kind, and any other malformed `/facts` command is only
    /// `/facts …`.
    #[test]
    fn a_facts_add_is_redacted_however_it_is_spaced() {
        for (cmd, want) in [
            (
                " /facts add phone digits 5550102345",
                "/facts add phone digits [value]",
            ),
            (
                "\n/facts add phone digits 5550102345\n",
                "/facts add phone digits [value]",
            ),
            (
                "/facts\tadd  phone\tdigits 5550102345",
                "/facts add phone digits [value]",
            ),
            ("/facts add 5550102345 digits", "/facts add [value]"),
            ("/facts add Phone text a\tb", "/facts add [value]"),
            ("/facts add phone number 5550102345", "/facts add [value]"),
            (
                "/facts add 5550102345 digits 5550102345",
                "/facts add [value]",
            ),
            ("/facts addphone digits 5550102345", "/facts …"),
            ("/factsadd phone digits 5550102345", "/facts …"),
            ("/facts edit phone 5550102345", "/facts …"),
            ("/facts rm 5550102345 now", "/facts …"),
            ("  /facts rm phone ", "/facts rm phone"),
        ] {
            let got = redact(cmd);
            assert_eq!(got, want, "{cmd:?}");
            assert!(!got.contains("555010"), "{cmd:?}: {got}");
        }
    }

    /// A5: the base64 forms of a value, standard and URL-safe, padded or not, at each of the
    /// three alignments, are found and scrubbed; a digits fact's digit-only value too.
    #[test]
    fn base64_forms_are_found_and_scrubbed_at_every_alignment() {
        use base64::engine::general_purpose::{
            STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD,
        };
        use base64::Engine;
        // `?>~` puts `/` and `+` (`_` and `-` URL-safe) into the encoding.
        let facts = [
            fact("token", FactKind::Text, "pass?>~word>>key"),
            fact("phone", FactKind::Digits, "+1 (555) 010-2345"),
        ];
        let engines: [&base64::engine::GeneralPurpose; 4] =
            [&STANDARD, &STANDARD_NO_PAD, &URL_SAFE, &URL_SAFE_NO_PAD];
        let standard = STANDARD.encode("pass?>~word>>key");
        let url = URL_SAFE.encode("pass?>~word>>key");
        assert!(
            standard.contains(['+', '/']) && url.contains(['-', '_']),
            "{standard} {url}"
        );
        for (name, secret) in [("token", "pass?>~word>>key"), ("phone", "15550102345")] {
            for engine in engines {
                for prefix in ["", "u", "us", "user:"] {
                    let encoded = engine.encode(format!("{prefix}{secret} and more"));
                    let text = format!("Authorization: Basic {encoded}");
                    assert_eq!(found(&facts, &text, None), [name], "{text}");
                    let out = scrub(&facts, &text);
                    assert!(out.contains(&format!("[fact:{name}]")), "{out}");
                    // The characters only the value makes are gone.
                    let k = prefix.len() % 3;
                    let middle = &encoded[(prefix.len() + 2) * 4 / 3..][..8];
                    assert!(!out.contains(middle), "{k} {middle}: {out}");
                }
            }
        }
    }

    /// D6: NFKC over the whole text, not character by character: a value written decomposed
    /// (NFD Hangul, as macOS names files) matches it composed, and the other way round; where
    /// the match cannot be placed, the whole text is withheld.
    #[test]
    fn decomposed_text_matches_composed_values() {
        use unicode_normalization::UnicodeNormalization;
        let nfc = "서울시 강남구 테헤란로";
        let nfd: String = nfc.nfd().collect();
        assert_ne!(nfd, nfc);
        for (value, text) in [(nfd.as_str(), nfc), (nfc, nfd.as_str())] {
            let facts = [fact("addr", FactKind::Text, value)];
            let text = format!("send to {text} today");
            assert_eq!(found(&facts, &text, None), ["addr"], "{text:?}");
            let out = scrub(&facts, &text);
            assert!(!out.contains("서울") && !out.contains(&nfd[..6]), "{out}");
            assert!(out.contains("[fact:addr]") || out.contains("addr"), "{out}");
        }
        // U+061C, a direction mark, hides no fact either.
        let phone = [fact("phone", FactKind::Digits, "5550102345")];
        assert_eq!(found(&phone, "555\u{061c}0102345", None), ["phone"]);
    }

    #[test]
    fn invisible_characters_hide_no_fact() {
        // R40: control, direction and zero-width characters count as absent, matching and
        // scrubbing; a line break still counts as the space it is.
        let facts = [
            fact("phone", FactKind::Digits, "5550102345"),
            fact("addr", FactKind::Text, "12 Oak Street, Springfield"),
            fact("mail", FactKind::Email, "owner@example.com"),
        ];
        for (text, name) in [
            ("555\u{202e}0102345", "phone"),
            ("555\u{200b}010\u{feff}2345", "phone"),
            ("555\u{7}010\u{2066}2345", "phone"),
            ("12 Oak\u{200d} Street,\u{2060} Spring\u{200f}field", "addr"),
            ("12 Oak Street,\nSpringfield", "addr"),
            ("owner\u{202d}@exam\u{200c}ple.com", "mail"),
        ] {
            assert_eq!(found(&facts, text, None), [name], "{text:?}");
            let out = scrub(&facts, &format!("x {text} y"));
            assert_eq!(out, format!("x [fact:{name}] y"), "{text:?}");
        }
        // An invisible character is no separator: digits on either side of a letter still are not.
        assert!(found(&facts, "555\u{202e}x0102345", None).is_empty());
    }

    #[test]
    fn debug_never_shows_a_value() {
        let f = fact("phone", FactKind::Digits, "5550102345");
        let shown = format!("{f:?} {:?}", std::slice::from_ref(&f));
        assert!(!shown.contains("5550102345"), "{shown}");
        assert!(shown.contains("10"), "{shown}");
        assert!(!f.public().to_string().contains("5550102345"));
        assert!(!f.line().contains("5550102345"));
    }
}
