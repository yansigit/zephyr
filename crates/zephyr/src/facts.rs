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

/// Whether `c` can sit between the digits of one written number.
fn separator(c: char) -> bool {
    matches!(c, ' ' | '-' | '.' | '(' | ')' | '/' | '\u{a0}')
}

/// The digit runs in `text`, as (byte start, byte end, digits): a run is digits with the
/// separators phone numbers and card numbers are written with between them.
fn runs(text: &str) -> Vec<(usize, usize, String)> {
    let digit = |c: char| -> Option<char> {
        let mut it = c
            .to_string()
            .nfkc()
            .collect::<String>()
            .chars()
            .collect::<Vec<_>>()
            .into_iter();
        match (it.next(), it.next()) {
            (Some(d), None) if d.is_ascii_digit() => Some(d),
            _ => None,
        }
    };
    let mut out = Vec::new();
    let mut cur: Option<(usize, usize, String)> = None;
    for (i, c) in text.char_indices() {
        match (digit(c), &mut cur) {
            (Some(d), Some(run)) => {
                run.1 = i + c.len_utf8();
                run.2.push(d);
            }
            (Some(d), None) => cur = Some((i, i + c.len_utf8(), d.to_string())),
            (None, Some(_)) if separator(c) => {}
            (None, _) => out.extend(cur.take()),
        }
    }
    out.extend(cur);
    out
}

/// Whether the digits `run` are the digits `value` written another way: the same, inside a
/// longer number, or the number without its country code (up to three digits).
fn same_number(value: &str, run: &str) -> bool {
    run.contains(value)
        || (run.len() >= MIN_DIGITS && run.len() + 3 >= value.len() && value.ends_with(run))
}

/// `value` percent-encoded, with a space as `%20` and as `+`, lower case: how it sits in a URL
/// or a form body.
fn encoded_forms(value: &str) -> Vec<String> {
    let enc = |plus: bool| -> String {
        let mut out = String::new();
        for b in value.nfkc().collect::<String>().bytes() {
            match b {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(b as char)
                }
                b' ' if plus => out.push('+'),
                _ => out.push_str(&format!("%{b:02x}")),
            }
        }
        out.to_lowercase()
    };
    let mut forms = vec![enc(false)];
    if value.contains(' ') {
        forms.push(enc(true));
    }
    forms.retain(|f| f != &value.to_lowercase());
    forms
}

/// `s` as (character, start byte, end byte in `s`) after NFKC, lower case and collapsing runs of
/// whitespace to one space, so a match in it maps back to the text it came from.
fn folded(s: &str) -> Vec<(char, usize, usize)> {
    let mut out: Vec<(char, usize, usize)> = Vec::new();
    for (i, c) in s.char_indices() {
        let end = i + c.len_utf8();
        for n in c.to_string().nfkc().flat_map(char::to_lowercase) {
            match (n.is_whitespace(), out.last_mut()) {
                (true, Some(last)) if last.0 == ' ' => last.2 = end,
                (true, _) => out.push((' ', i, end)),
                (false, _) => out.push((n, i, end)),
            }
        }
    }
    out
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
    if f.kind == FactKind::Digits {
        let value = digits_of(&f.value);
        for (a, b, run) in runs(text) {
            if same_number(&value, &run) {
                out.push((a, b));
            }
        }
    }
    // The value as written, in the text's own terms: case, spacing and Unicode forms ignored.
    let pattern: Vec<char> = {
        let p: Vec<char> = folded(&f.value).into_iter().map(|(c, ..)| c).collect();
        let (from, to) = (
            p.iter().position(|c| *c != ' ').unwrap_or(p.len()),
            p.iter().rposition(|c| *c != ' ').map_or(0, |i| i + 1),
        );
        p[from..to.max(from)].to_vec()
    };
    let hay = folded(text);
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
    // Percent-encoded: hex digits in either case, so compare lower case (ASCII keeps offsets).
    let lower = text.to_ascii_lowercase();
    for form in encoded_forms(&f.value) {
        out.extend(lower.match_indices(&form).map(|(i, m)| (i, i + m.len())));
    }
    for form in crate::vault::base64_forms(&f.value) {
        out.extend(text.match_indices(&form).map(|(i, m)| (i, i + m.len())));
    }
    out
}

/// The names of the facts in `text`, once each. A fact whose `allow` names `target` is skipped.
pub fn found<'a>(facts: &'a [Fact], text: &str, target: Option<&str>) -> Vec<&'a str> {
    let mut names: Vec<&str> = Vec::new();
    for f in facts {
        let skip = target.is_some_and(|t| allowed(f, t));
        if !skip && !names.contains(&f.name.as_str()) && !spans(f, text).is_empty() {
            names.push(&f.name);
        }
    }
    names
}

/// `text` with each fact found in it replaced by `[fact:NAME]`, for audit rows and notices.
pub fn scrub(facts: &[Fact], text: &str) -> String {
    let mut marks: Vec<(usize, usize, &str)> = Vec::new();
    for f in facts {
        marks.extend(
            spans(f, text)
                .into_iter()
                .map(|(a, b)| (a, b, f.name.as_str())),
        );
    }
    // Leftmost first, the longest of those starting together; a span inside another is dropped.
    marks.sort_by_key(|&(a, b, _)| (a, std::cmp::Reverse(b)));
    let (mut out, mut at) = (String::new(), 0);
    for (a, b, name) in marks {
        if a < at {
            continue;
        }
        out.push_str(&text[at..a]);
        out.push_str(&format!("[fact:{name}]"));
        at = b;
    }
    out.push_str(&text[at..]);
    out
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
pub fn command(path: &Path, arg: &str, strong: bool) -> String {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let edit = |f: &dyn Fn(&mut Vec<Fact>) -> anyhow::Result<String>| match edit(path, f) {
        Ok(text) => format!("[zephyr] {text}"),
        Err(e) => format!("[zephyr] {e:#}"),
    };
    let weak = "[zephyr] loosening the check for a private fact needs a strong channel";
    match words[..] {
        [] | ["list"] => load(path).map_or_else(|e| format!("[zephyr] {e:#}"), |f| list(&f)),
        ["rm" | "allow", ..] if !strong => weak.into(),
        ["rm", name] => edit(&|f| match remove(f, name) {
            true => Ok(format!("removed the fact {name}")),
            false => bail!("no fact named {name}"),
        }),
        ["allow", name, target] => edit(&|f| allow(f, name, target, true)),
        ["disallow", name, target] => edit(&|f| allow(f, name, target, false)),
        ["add" | "edit", ..] => "[zephyr] values never go through a chat: in zephyr attach, \
            /facts opens a form; on the host, run `zephyr facts add NAME --kind text|digits|email`"
            .into(),
        _ => "[zephyr] usage: /facts, /facts rm NAME, /facts allow NAME TARGET, \
              /facts disallow NAME TARGET"
            .into(),
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
    fn debug_never_shows_a_value() {
        let f = fact("phone", FactKind::Digits, "5550102345");
        let shown = format!("{f:?} {:?}", std::slice::from_ref(&f));
        assert!(!shown.contains("5550102345"), "{shown}");
        assert!(shown.contains("10"), "{shown}");
        assert!(!f.public().to_string().contains("5550102345"));
        assert!(!f.line().contains("5550102345"));
    }
}
