//! The credential vault (DESIGN §4.15): website logins and tokens the owner saves in
//! `<state>/vault.toml` (root's, 0600, out of the agent's reach), which the agent uses by name.
//!
//! - A login is typed into the browser only when the page is on the login's own site, and only
//!   once the owner allowed that login (`granted`).
//! - A token is set in the environment of one `exec` command, only when the owner allowed it for
//!   every program the command runs (`programs`); what the command prints comes back with the
//!   value replaced by `[vault:NAME]`.
//! - Grants come only from the owner: their own `/vault grant`, or `/ok N` on a proposal the
//!   daemon stored (`tools::ask_owner`). Nothing the agent writes is ever read as one.
//!
//! The file is read fresh on every use, so an edit needs no reload, and edited with toml_edit, so
//! its comments survive. It is not encrypted: a passphrase would stop unattended restarts.

use crate::config::Config;
use crate::store::Store;
use anyhow::{bail, Context};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table};
use zephyr_policy::tools::VaultAction;

pub const FILE: &str = "vault.toml";

/// `<state>/vault.toml`, beside secrets.env.
pub fn path(cfg: &Config) -> PathBuf {
    cfg.state_dir().join(FILE)
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Login,
    Token,
}

impl Kind {
    fn key(self) -> &'static str {
        match self {
            Kind::Login => "login",
            Kind::Token => "token",
        }
    }
}

/// One saved credential. A login uses `site`, `username`, `password`, `totp` (a BASE32 seed, empty
/// for none) and `granted`; a token uses `value` and `programs`. Never printed: `Debug` shows only
/// the secrets' lengths.
#[derive(Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Entry {
    pub kind: Kind,
    pub name: String,
    pub site: String,
    pub username: String,
    pub password: String,
    pub totp: String,
    pub granted: bool,
    pub value: String,
    pub programs: Vec<String>,
    /// The exact `host:port`s a token is swapped in on (DESIGN §4.21); nowhere else.
    pub hosts: Vec<String>,
    /// Also swapped in a request's path and query.
    pub url: bool,
    /// Handed to the program as its real value, as before the broker.
    pub raw: bool,
}

impl std::fmt::Debug for Entry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Entry")
            .field("kind", &self.kind)
            .field("name", &self.name)
            .field("site", &self.site)
            .field("username", &self.username)
            .field("password", &self.password.len())
            .field("totp", &self.totp.len())
            .field("granted", &self.granted)
            .field("value", &self.value.len())
            .field("programs", &self.programs)
            .field("hosts", &self.hosts)
            .field("url", &self.url)
            .field("raw", &self.raw)
            .finish()
    }
}

impl Entry {
    pub fn check(&self) -> anyhow::Result<()> {
        let name = &self.name;
        match self.kind {
            Kind::Login => {
                if !login_name(name) {
                    bail!("{name:?} is not a login name: lowercase letters, digits, '-' and '_', at most 64");
                }
                if origin(&self.site).is_none() {
                    bail!(
                        "login {name}: the site {:?} is not an http(s) address",
                        self.site
                    );
                }
                if self.password.is_empty() {
                    bail!("login {name} has no password");
                }
                if !self.totp.is_empty() {
                    seed(&self.totp).with_context(|| format!("login {name}: the TOTP seed"))?;
                }
            }
            Kind::Token => {
                if !token_name(name) {
                    bail!("{name:?} is not a token name: an environment variable name like GH_TOKEN, at most 64");
                }
                if self.value.is_empty() {
                    bail!("token {name} has no value");
                }
            }
        }
        Ok(())
    }

    /// What anyone may see: no password, seed or value.
    pub fn public(&self) -> Value {
        json!({
            "kind": self.kind,
            "name": self.name,
            "site": self.site,
            "username": self.username,
            "totp": !self.totp.is_empty(),
            "granted": self.granted,
            "programs": self.programs,
        })
    }

    /// One line for `/vault`, the agent's `vault list` and `zephyr vault list`.
    pub fn line(&self) -> String {
        match self.kind {
            Kind::Login => {
                let user = match self.username.as_str() {
                    "" => String::new(),
                    u => format!("{u} on "),
                };
                let code = if self.totp.is_empty() {
                    ""
                } else {
                    ", with a one-time code"
                };
                let grant = if self.granted {
                    "filling allowed"
                } else {
                    "the first fill needs the owner's OK"
                };
                format!("login {}: {user}{}{code}; {grant}", self.name, self.site)
            }
            Kind::Token => match self.programs.as_slice() {
                [] => format!("token {}: not allowed for any program yet", self.name),
                p => format!("token {}: allowed for {}", self.name, p.join(", ")),
            },
        }
    }
}

/// `[a-z0-9][a-z0-9_-]{0,63}`
pub fn login_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_' || *c == b'-')
}

/// `[A-Z_][A-Z0-9_]{0,63}`
pub fn token_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=64).contains(&b.len())
        && (b[0].is_ascii_uppercase() || b[0] == b'_')
        && b.iter()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == b'_')
}

/// A program a token may be granted for: a plain file name, so the grant command reads back the
/// same and `$X` or a quote never stands in for one.
pub fn program_name(p: &str) -> bool {
    (1..=64).contains(&p.len())
        && !p.starts_with('-')
        && p.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._+-".contains(&b))
}

/// The origin (scheme, host, port) of an http(s) address; `None` for anything else.
pub fn origin(address: &str) -> Option<url::Origin> {
    let u = url::Url::parse(address.trim()).ok()?;
    let o = u.origin();
    (matches!(u.scheme(), "http" | "https") && o.is_tuple()).then_some(o)
}

/// Whether `page` is on `site`: the same scheme, host and port, exactly.
pub fn same_origin(site: &str, page: &str) -> bool {
    matches!((origin(site), origin(page)), (Some(a), Some(b)) if a == b)
}

/// The vault file, as parsed. Its entries are read from the document each time, so an edit goes
/// back with every comment where it was.
pub struct Vault {
    doc: DocumentMut,
}

impl Vault {
    pub fn parse(text: &str) -> anyhow::Result<Vault> {
        // Only the line: the parser's own message quotes the text around the error, a value maybe.
        let doc: DocumentMut = text.parse().map_err(|e: toml_edit::TomlError| {
            let at = e.span().map_or(0, |s| s.start).min(text.len());
            let line = text.as_bytes()[..at]
                .iter()
                .filter(|&&b| b == b'\n')
                .count()
                + 1;
            anyhow::anyhow!("vault.toml is not valid TOML (line {line})")
        })?;
        for key in ["login", "token"] {
            if doc
                .get(key)
                .is_some_and(|i| i.as_array_of_tables().is_none())
            {
                bail!("vault.toml: `{key}` must be [[{key}]] tables");
            }
        }
        let v = Vault { doc };
        let mut seen = std::collections::HashSet::new();
        for e in v.entries() {
            e.check().context("vault.toml")?;
            if !seen.insert(e.name.clone()) {
                bail!("vault.toml has two entries named {}", e.name);
            }
        }
        Ok(v)
    }

    /// An empty vault when there is no file yet.
    pub fn load(path: &Path) -> anyhow::Result<Vault> {
        match std::fs::read_to_string(path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vault::parse(""),
            r => Vault::parse(&r.with_context(|| format!("reading {}", path.display()))?),
        }
    }

    pub fn save(&self, path: &Path) -> anyhow::Result<()> {
        crate::host::conf::replace_private(path, &self.to_string())
    }

    pub fn entries(&self) -> Vec<Entry> {
        [Kind::Login, Kind::Token]
            .into_iter()
            .flat_map(|kind| {
                let tables = self.doc.get(kind.key()).and_then(Item::as_array_of_tables);
                tables
                    .into_iter()
                    .flat_map(|a| a.iter())
                    .map(move |t| read(kind, t))
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<Entry> {
        self.entries().into_iter().find(|e| e.name == name)
    }

    fn table(&mut self, kind: Kind, name: &str) -> Option<&mut Table> {
        self.doc
            .get_mut(kind.key())
            .and_then(Item::as_array_of_tables_mut)?
            .iter_mut()
            .find(|t| t.get("name").and_then(Item::as_str) == Some(name))
    }

    /// Adds `new`, or replaces the entry of that name. An empty password, seed or value keeps the
    /// one saved. Grants are never set here, only kept: a login's survives while its site stays
    /// the same, a token's always. Returns whether the entry is new.
    pub fn put(&mut self, mut new: Entry) -> anyhow::Result<bool> {
        let old = self.get(&new.name);
        match &old {
            Some(old) if old.kind != new.kind => {
                bail!("{} is already saved as a {}", new.name, old.kind.key())
            }
            Some(old) => {
                for (field, kept) in [
                    (&mut new.password, &old.password),
                    (&mut new.totp, &old.totp),
                    (&mut new.value, &old.value),
                ] {
                    if field.is_empty() {
                        field.clone_from(kept);
                    }
                }
                new.granted = old.granted && same_origin(&old.site, &new.site);
                new.programs.clone_from(&old.programs);
            }
            None => {
                new.granted = false;
                new.programs.clear();
            }
        }
        new.check()?;
        let kind = new.kind;
        let t = match self.table(kind, &new.name) {
            Some(t) => t,
            None => {
                let list = self
                    .doc
                    .entry(kind.key())
                    .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
                    .as_array_of_tables_mut()
                    .context("vault.toml")?;
                list.push(Table::new());
                list.iter_mut().last().expect("just pushed")
            }
        };
        set(t, "name", &new.name);
        match kind {
            Kind::Login => {
                set(t, "site", new.site.trim());
                set(t, "username", &new.username);
                set(t, "password", &new.password);
                match new.totp.as_str() {
                    "" => drop(t.remove("totp")),
                    seed => set(t, "totp", seed),
                }
                set(t, "granted", new.granted);
            }
            Kind::Token => {
                set(t, "value", &new.value);
                set(t, "programs", Array::from_iter(new.programs.iter()));
            }
        }
        Ok(old.is_none())
    }

    pub fn remove(&mut self, name: &str) -> bool {
        let mut gone = false;
        for kind in [Kind::Login, Kind::Token] {
            if let Some(list) = self
                .doc
                .get_mut(kind.key())
                .and_then(Item::as_array_of_tables_mut)
            {
                let before = list.len();
                list.retain(|t| t.get("name").and_then(Item::as_str) != Some(name));
                gone |= list.len() < before;
            }
        }
        gone
    }

    /// The owner allows a login's fills, or a token's use with `programs` (added to those it has).
    pub fn grant(&mut self, kind: Kind, name: &str, programs: &[&str]) -> anyhow::Result<String> {
        let e = self
            .get(name)
            .filter(|e| e.kind == kind)
            .with_context(|| format!("no {} named {name} in the vault", kind.key()))?;
        let t = self.table(kind, name).expect("found above");
        match kind {
            Kind::Login => {
                set(t, "granted", true);
                Ok(format!(
                    "the agent may now fill the login {name} on {}",
                    e.site
                ))
            }
            Kind::Token => {
                if let Some(bad) = programs.iter().find(|p| !program_name(p)) {
                    bail!("{bad:?} is not a program name");
                }
                let mut all = e.programs;
                for p in programs {
                    if !all.iter().any(|a| a == p) {
                        all.push(p.to_string());
                    }
                }
                set(t, "programs", Array::from_iter(all.iter()));
                Ok(format!(
                    "the agent may now use {name} with {}",
                    all.join(", ")
                ))
            }
        }
    }

    /// Takes back a login's grant, or a token's for one program (all of them when `None`).
    pub fn revoke(&mut self, name: &str, program: Option<&str>) -> anyhow::Result<String> {
        let e = self
            .get(name)
            .with_context(|| format!("no entry named {name} in the vault"))?;
        let t = self.table(e.kind, name).expect("found above");
        match (e.kind, program) {
            (Kind::Login, Some(_)) => bail!("a login has no programs; /vault revoke {name}"),
            (Kind::Login, None) => {
                set(t, "granted", false);
                Ok(format!("the agent must ask again before filling {name}"))
            }
            (Kind::Token, None) => {
                set(t, "programs", Array::new());
                Ok(format!("{name} is no longer allowed for any program"))
            }
            (Kind::Token, Some(p)) => {
                let left: Vec<&String> = e.programs.iter().filter(|a| *a != p).collect();
                if left.len() == e.programs.len() {
                    bail!("{name} was not allowed for {p}");
                }
                set(t, "programs", Array::from_iter(left));
                Ok(format!("{name} is no longer allowed for {p}"))
            }
        }
    }

    /// Every value a command or a page could echo back, by entry name, for [`scrub`].
    pub fn secrets(&self) -> Vec<(String, String)> {
        self.entries()
            .into_iter()
            .map(|e| match e.kind {
                Kind::Login => (e.name, e.password),
                Kind::Token => (e.name, e.value),
            })
            .collect()
    }

    /// Names, kinds, sites, usernames and grants; never a value.
    pub fn list(&self) -> String {
        let lines: Vec<String> = self.entries().iter().map(Entry::line).collect();
        if lines.is_empty() {
            return "[zephyr] the vault is empty".into();
        }
        format!("[zephyr] vault:\n{}", lines.join("\n"))
    }
}

impl std::fmt::Display for Vault {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.doc.fmt(f)
    }
}

/// Sets `key` to `v`, keeping the comment around the old value.
fn set(t: &mut Table, key: &str, v: impl Into<toml_edit::Value>) {
    let mut v = v.into();
    if let Some(old) = t.get(key).and_then(Item::as_value) {
        *v.decor_mut() = old.decor().clone();
    }
    t[key] = Item::Value(v);
}

fn read(kind: Kind, t: &Table) -> Entry {
    let s = |k: &str| t.get(k).and_then(Item::as_str).unwrap_or("").to_string();
    let flag = |k: &str| t.get(k).and_then(Item::as_bool).unwrap_or(false);
    let list = |k: &str| -> Vec<String> {
        let items = t.get(k).and_then(Item::as_array).into_iter().flatten();
        items.filter_map(|v| v.as_str().map(String::from)).collect()
    };
    Entry {
        kind,
        name: s("name"),
        site: s("site"),
        username: s("username"),
        password: s("password"),
        totp: s("totp"),
        granted: flag("granted"),
        value: s("value"),
        programs: list("programs"),
        hosts: list("hosts"),
        url: flag("url"),
        raw: flag("raw"),
    }
}

/// One writer at a time in this process: the router's grants and the attach port's saves.
static EDIT: Mutex<()> = Mutex::new(());

/// Loads the vault, applies `f` and saves it, as one step.
pub fn edit<T>(path: &Path, f: impl FnOnce(&mut Vault) -> anyhow::Result<T>) -> anyhow::Result<T> {
    let _held = EDIT.lock().unwrap_or_else(|e| e.into_inner());
    let mut vault = Vault::load(path)?;
    let out = f(&mut vault)?;
    vault.save(path)?;
    Ok(out)
}

/// A value shorter than this is left in the text: a two-letter one would turn up everywhere.
/// shortcut: such a value is shown as is; refuse to save it if that ever matters.
const MIN_SCRUB: usize = 4;

/// `text` with every value in `secrets` replaced by `[vault:NAME]`, longest first, and so its
/// base64 forms (`curl -u x:$TOKEN -v` prints one); then any run of 8 or more characters of
/// either, as a cut-off or partial print leaves, masked as `auth::redact` masks a login's echo.
pub fn scrub(text: &str, secrets: &[(String, String)]) -> String {
    let mut forms: Vec<(&str, String)> = Vec::new();
    for (name, v) in secrets.iter().filter(|(_, v)| v.len() >= MIN_SCRUB) {
        forms.push((name.as_str(), v.clone()));
        forms.extend(base64_forms(v).into_iter().map(|f| (name.as_str(), f)));
    }
    forms.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let mut out = text.to_string();
    for (name, v) in &forms {
        if out.contains(v.as_str()) {
            out = out.replace(v.as_str(), &format!("[vault:{name}]"));
        }
    }
    let all: Vec<String> = forms.into_iter().map(|(_, v)| v).collect();
    crate::auth::redact(&out, &all)
}

/// The base64 text `value` always turns into, standard and URL-safe, wherever it sits in the
/// encoded bytes (`user:value` puts it at another offset): at each of the three byte alignments,
/// the characters that depend on `value` alone. A form shorter than 8 characters could be anything.
pub(crate) fn base64_forms(value: &str) -> Vec<String> {
    use base64::engine::general_purpose::{STANDARD_NO_PAD, URL_SAFE_NO_PAD};
    use base64::Engine;
    let mut out = Vec::new();
    for k in 0..3 {
        let mut bytes = vec![0u8; k];
        bytes.extend_from_slice(value.as_bytes());
        // Character i holds bits 6i..6i+6; keep those inside the value's bits 8k..8(k+n).
        let (first, end) = ((8 * k).div_ceil(6), 8 * bytes.len() / 6);
        for engine in [&STANDARD_NO_PAD, &URL_SAFE_NO_PAD] {
            let text = engine.encode(&bytes);
            if end >= first + 8 && !out.contains(&text[first..end].to_string()) {
                out.push(text[first..end].to_string());
            }
        }
    }
    out
}

/// [`scrub`] with the vault as it is now. A command re-adopted after a re-exec is scrubbed this
/// way too: no value is ever kept anywhere but the vault file. Fails closed: output the vault
/// cannot be checked against is not shown.
pub fn scrubbed(cfg: &Config, text: String) -> String {
    match Vault::load(&path(cfg)) {
        Ok(v) => scrub(&text, &v.secrets()),
        Err(_) => "[output withheld: the vault could not be read]".into(),
    }
}

/// A BASE32 TOTP seed as the key bytes: case, spaces, dashes and missing padding are forgiven.
fn seed(text: &str) -> anyhow::Result<Vec<u8>> {
    let mut s: String = text
        .chars()
        .filter(|c| !c.is_whitespace() && *c != '-' && *c != '=')
        .map(|c| c.to_ascii_uppercase())
        .collect();
    while !s.len().is_multiple_of(8) {
        s.push('=');
    }
    let key = data_encoding::BASE32
        .decode(s.as_bytes())
        .map_err(|_| anyhow::anyhow!("not a BASE32 seed"))?;
    if key.is_empty() {
        bail!("the seed is empty");
    }
    Ok(key)
}

/// RFC 6238 with HMAC-SHA1 and 30 s steps: the code for `unix` seconds, `digits` long.
fn hotp(key: &[u8], unix: u64, digits: u32) -> String {
    use ring::hmac;
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, key),
        &(unix / 30).to_be_bytes(),
    );
    let h = tag.as_ref();
    let at = (h[h.len() - 1] & 0x0f) as usize;
    let n = u32::from_be_bytes([h[at] & 0x7f, h[at + 1], h[at + 2], h[at + 3]]);
    format!("{:0width$}", n % 10u32.pow(digits), width = digits as usize)
}

/// The six-digit one-time code for a login's seed at `unix` seconds.
pub fn totp(seed_text: &str, unix: u64) -> anyhow::Result<String> {
    Ok(hotp(&seed(seed_text)?, unix, 6))
}

/// The owner's `/vault …`, on any channel (DESIGN §4.15). An approved proposal arrives here as
/// the owner's own `/vault grant …`. Values are never taken in a chat message.
pub fn command(path: &Path, arg: &str, may_approve: bool) -> Result<String, String> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let edit = |f: &dyn Fn(&mut Vault) -> anyhow::Result<String>| match edit(path, f) {
        Ok(text) => Ok(format!("[zephyr] {text}")),
        Err(e) => Err(format!("[zephyr] {e:#}")),
    };
    match words[..] {
        [] | ["list"] => {
            (Vault::load(path).map(|v| v.list())).map_err(|e| format!("[zephyr] {e:#}"))
        }
        ["grant", ..] if !may_approve => Err(crate::tools::NEEDS_PASSKEY.into()),
        ["rm", name] => edit(&|v| match v.remove(name) {
            true => Ok(format!("removed {name} from the vault")),
            false => bail!("no entry named {name} in the vault"),
        }),
        ["revoke", name] => edit(&|v| v.revoke(name, None)),
        ["revoke", name, program] => edit(&|v| v.revoke(name, Some(program))),
        ["grant", "login", name] => edit(&|v| v.grant(Kind::Login, name, &[])),
        ["grant", "token", name, ref programs @ ..] if !programs.is_empty() => {
            edit(&|v| v.grant(Kind::Token, name, programs))
        }
        ["add" | "edit", ..] => Err(
            "[zephyr] values never go through a chat: in zephyr attach, \
            /vault opens a form; on the host, run `zephyr vault add login NAME --site URL` or \
            `zephyr vault add token NAME`"
                .into(),
        ),
        _ => Err(
            "[zephyr] usage: /vault, /vault rm NAME, /vault revoke NAME [PROGRAM], \
              /vault grant login NAME, /vault grant token NAME PROGRAM.."
                .into(),
        ),
    }
}

/// What `/ok` does to a vault proposal, in the owner's notice.
const ALLOWS: &str = "allows it from now on";

/// The environment main's `exec` gets for the vault tokens it names (DESIGN §4.15): every token
/// must be allowed for every program the command runs. `Err` is the call's whole answer, and the
/// command does not run; a missing grant is proposed to the owner first.
pub fn exec_env(
    cfg: &Config,
    store: &Store,
    command: &str,
    names: &[String],
) -> Result<Vec<(String, String)>, String> {
    let vault = Vault::load(&path(cfg)).map_err(|e| format!("not run: {e:#}"))?;
    let programs = zephyr_policy::policy::programs(command);
    if let Some(odd) = programs.iter().find(|p| !program_name(p)) {
        return Err(format!(
            "not run: {odd:?} is not a plain program name, so the vault cannot tell what would \
             get the token; name the program itself"
        ));
    }
    if programs.is_empty() {
        return Err("not run: the command runs no program to give the token to".into());
    }
    let (mut env, mut asked) = (Vec::new(), Vec::new());
    for name in names {
        let Some(token) = vault.get(name).filter(|e| e.kind == Kind::Token) else {
            return Err(format!(
                "not run: no token named {name} in the vault; `vault request` it from the owner"
            ));
        };
        let missing: Vec<&str> = programs
            .iter()
            .filter(|p| !token.programs.contains(p))
            .map(String::as_str)
            .collect();
        if missing.is_empty() {
            env.push((token.name, token.value));
            continue;
        }
        let command = format!("/vault grant token {name} {}", missing.join(" "));
        let ask = format!("to use the token {name} with {}", missing.join(", "));
        match crate::tools::ask_owner(store, &command, &ask, ALLOWS) {
            Ok(id) => asked.push(format!("{name} with {} (#{id})", missing.join(", "))),
            Err(e) => return Err(format!("not run: asking the owner failed: {e}")),
        }
    }
    match asked.is_empty() {
        true => Ok(env),
        false => Err(format!(
            "not run: it needs the owner's OK to use {}. Tell the owner and end your turn; run it \
             again once they allow it.",
            asked.join("; ")
        )),
    }
}

/// The agent's `vault` call `call_id` (main only), answered through the gateway file: at once,
/// except a fill, which runs the browser from a task of its own.
pub fn agent(cfg: &Arc<Config>, store: &Store, call_id: &str, action: VaultAction) {
    let answer = |text: &str| crate::tools::answer_gateway(cfg, call_id, text);
    let vault = match Vault::load(&path(cfg)) {
        Ok(v) => v,
        Err(e) => return answer(&format!("{e:#}")),
    };
    match action {
        VaultAction::List => answer(&vault.list()),
        VaultAction::Request {
            name,
            kind,
            site,
            why,
        } => answer(&request(store, &name, &kind, site.as_deref(), &why)),
        VaultAction::Fill {
            name,
            user,
            password,
            totp,
        } => {
            let Some(login) = vault.get(&name).filter(|e| e.kind == Kind::Login) else {
                return answer(&format!(
                    "no login named {name} in the vault; `request` it from the owner"
                ));
            };
            if !login.granted {
                let command = format!("/vault grant login {name}");
                let ask = format!("to fill the login {name} on {}", login.site);
                return answer(&match crate::tools::ask_owner(store, &command, &ask, ALLOWS) {
                    Ok(id) => format!(
                        "not filled: the first fill of {name} needs the owner's OK, asked as #{id}. \
                         Tell the owner and end your turn; fill again once they allow it."
                    ),
                    Err(e) => format!("not filled: asking the owner failed: {e}"),
                });
            }
            let (cfg, id) = (cfg.clone(), call_id.to_string());
            tokio::spawn(async move {
                let text = fill(&cfg, &login, [user, password, totp]).await;
                crate::tools::answer_gateway(&cfg, &id, &text);
            });
        }
    }
}

/// Asks the owner to save a credential the agent needs, with the exact commands that do it.
fn request(store: &Store, name: &str, kind: &str, site: Option<&str>, why: &str) -> String {
    let (what, tui, host) = match kind {
        "login" => {
            if !login_name(name) {
                return format!(
                    "{name:?} is not a login name: lowercase letters, digits, '-', '_'"
                );
            }
            let Some(site) = site.and_then(origin) else {
                return "`site` must be an http(s) address, e.g. https://github.com".into();
            };
            let site = site.ascii_serialization();
            (
                format!("a login for {site} as {name}"),
                format!("/vault add login {name} {site}"),
                format!("zephyr vault add login {name} --site {site}"),
            )
        }
        _ => {
            if !token_name(name) {
                return format!("{name:?} is not a token name: an env var name like GH_TOKEN");
            }
            (
                format!("a token as {name}"),
                format!("/vault add token {name}"),
                format!("zephyr vault add token {name}"),
            )
        }
    };
    // The agent's words, marked as such and on this one line, so they cannot pass as zephyr's.
    let why: String = why.split_whitespace().collect::<Vec<_>>().join(" ");
    let why = match why.chars().take(200).collect::<String>() {
        w if w.is_empty() => String::new(),
        w => format!(" The agent says: \"{w}\""),
    };
    let text = format!(
        "[zephyr] the agent asks you to save {what}: {tui} in zephyr attach, or `{host}` on the \
         host. Never paste it into the chat.{why}"
    );
    match store.notify_last_owner(&text) {
        Ok(()) => "asked the owner to save it. Tell them and end your turn: a save in zephyr \
                   attach comes back to you as a message, or the owner tells you."
            .into(),
        Err(e) => format!("asking the owner failed: {e}"),
    }
}

/// The fields a fill types, as `(label, selector, value)`, the one-time code made for `unix`.
fn fields(
    login: &Entry,
    [user, password, totp]: &[Option<String>; 3],
    unix: u64,
) -> Result<Vec<(&'static str, String, String)>, String> {
    let mut out = Vec::new();
    if let Some(sel) = user {
        if login.username.is_empty() {
            return Err(format!(
                "not filled: login {} has no username saved",
                login.name
            ));
        }
        out.push(("username", sel.clone(), login.username.clone()));
    }
    if let Some(sel) = password {
        out.push(("password", sel.clone(), login.password.clone()));
    }
    if let Some(sel) = totp {
        let code = match login.totp.as_str() {
            "" => {
                return Err(format!(
                    "not filled: login {} has no one-time code seed",
                    login.name
                ))
            }
            seed => self::totp(seed, unix).map_err(|e| format!("not filled: {e:#}"))?,
        };
        out.push(("one-time code", sel.clone(), code));
    }
    // A selector is a positional argument of agent-browser: one that looks like a flag is refused.
    if out.iter().any(|(_, sel, _)| sel.starts_with('-')) {
        return Err("not filled: a selector cannot start with '-'".into());
    }
    // agent-browser reads `--x` as its own option wherever it stands, even after `--`, so such a
    // value would be typed wrong; the owner fills that one by hand.
    if let Some((label, _, _)) = out.iter().find(|(_, _, v)| v.starts_with("--")) {
        return Err(format!(
            "not filled: the saved {label} of {} starts with --, which agent-browser would read as \
             an option; ask the owner to type it",
            login.name
        ));
    }
    Ok(out)
}

/// The first http(s) address in agent-browser's `get url` output.
fn page_address(out: &str) -> Option<&str> {
    out.split_whitespace().find(|w| origin(w).is_some())
}

/// Where the browser's page is now, as `get url` says: `None` when that fails or is no http(s) page.
async fn page_now(cfg: &Config) -> Option<String> {
    let out = browser(cfg, &["get", "url"]).await.ok()?;
    page_address(&out).map(String::from)
}

/// Why a fill stops after typing `done` because the page left `site` (`now`), or `None` while the
/// page is still on it.
fn moved(site: &str, now: Option<&str>, done: &[&str]) -> Option<String> {
    if now.is_some_and(|p| same_origin(site, p)) {
        return None;
    }
    let on = now
        .and_then(origin)
        .map_or("an unknown page".into(), |o| o.ascii_serialization());
    Some(format!(
        "stopped: after the {} was typed the page was on {on}, not {site}; the value may have \
         reached that page. Submit nothing there, and tell the owner",
        done.join(" and ")
    ))
}

/// Types the login's fields into the page open in agent-browser, once the page is on its site.
/// The address is read again after each field, so a page that moves away gets no further field.
/// shortcut: agent-browser has no fill-if-origin, so a redirect in the moment before a fill still
/// gets that one value; then the agent is told, and submits nothing.
async fn fill(cfg: &Config, login: &Entry, selectors: [Option<String>; 3]) -> String {
    let fields = match fields(login, &selectors, crate::now() as u64) {
        Ok(f) => f,
        Err(e) => return e,
    };
    let mut secrets = vec![(login.name.clone(), login.password.clone())];
    secrets.extend(
        fields
            .iter()
            .map(|(_, _, v)| (login.name.clone(), v.clone())),
    );
    let page = match browser(cfg, &["get", "url"]).await {
        Ok(out) => page_address(&out).map(String::from),
        Err(e) => return format!("not filled: reading the page's address failed: {e}"),
    };
    let Some(page) = page.clone().filter(|p| same_origin(&login.site, p)) else {
        let on = page
            .as_deref()
            .and_then(origin)
            .map_or("no http(s) page".into(), |o| o.ascii_serialization());
        return format!(
            "not filled: the open page is on {on}, but {} is saved for {}",
            login.name, login.site
        );
    };
    let mut done: Vec<&str> = Vec::new();
    for (label, sel, value) in &fields {
        if let Err(e) = browser(cfg, &["fill", sel, value]).await {
            let done = match done.is_empty() {
                true => String::new(),
                false => format!("filled the {}, ", done.join(" and ")),
            };
            return scrub(&format!("{done}but not the {label}: {e}"), &secrets);
        }
        done.push(label);
        if let Some(stop) = moved(&login.site, page_now(cfg).await.as_deref(), &done) {
            return stop;
        }
    }
    format!("filled the {} on {page}", done.join(" and "))
}

/// How long one agent-browser command may take; a fill runs up to seven (an address read before
/// and after each field), normally well within the tool's wait.
const BROWSER_WAIT: Duration = Duration::from_secs(20);

/// `agent-browser ARGS` as the agent, as `exec` runs it: its stdout, or why it failed. The real
/// binary, by full path (`cfg.agent_browser`): the daemon's own fill never goes through the
/// approvals gate's wrapper that `agent-browser` on the agent's PATH is (approvals spec §4.4).
async fn browser(cfg: &Config, args: &[&str]) -> Result<String, String> {
    let mut cmd = tokio::process::Command::new(&cfg.agent_browser);
    cmd.args(args)
        .current_dir(&cfg.home)
        .env("HOME", &cfg.home)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true);
    crate::procs::for_agent(cmd.as_std_mut());
    let out = tokio::time::timeout(BROWSER_WAIT, cmd.output())
        .await
        .map_err(|_| {
            format!(
                "agent-browser did not answer in {} s",
                BROWSER_WAIT.as_secs()
            )
        })?
        .map_err(|e| format!("running agent-browser: {e}"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&out.stderr);
    let last = said
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("it failed");
    Err(last.chars().take(300).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# my logins
[[login]]
name = "github"
site = "https://github.com"   # fill only here
username = "aiden"
password = "hunter22"
totp = "gezd gnbv gy3t qojq gezd gnbv gy3t qojq"
granted = true

[[token]]
name = "GH_TOKEN"
value = "ghp_secret123"
programs = ["gh"]
"#;

    #[test]
    fn parses_checks_and_lists_without_values() {
        let v = Vault::parse(SAMPLE).unwrap();
        let e = v.entries();
        assert_eq!(e.len(), 2);
        assert_eq!((e[0].kind, e[0].name.as_str()), (Kind::Login, "github"));
        assert!(e[0].granted);
        assert_eq!(e[1].programs, ["gh"]);
        let list = v.list();
        for secret in ["hunter22", "ghp_secret123", "GEZD", "gezd"] {
            assert!(!list.contains(secret), "{list}");
            assert!(!format!("{e:?}").contains(secret));
            assert!(!e.iter().any(|e| e.public().to_string().contains(secret)));
        }
        assert!(list.contains("login github: aiden on https://github.com, with a one-time code"));
        assert!(list.contains("token GH_TOKEN: allowed for gh"), "{list}");
        assert!(Vault::parse("").unwrap().list().contains("empty"));

        for bad in [
            "[[login]]\nname = \"GitHub\"\nsite = \"https://x\"\npassword = \"p\"",
            "[[login]]\nname = \"x\"\nsite = \"ftp://x\"\npassword = \"p\"",
            "[[login]]\nname = \"x\"\nsite = \"https://x\"",
            "[[token]]\nname = \"gh_token\"\nvalue = \"v\"",
            "[[token]]\nname = \"A\"\nvalue = \"v\"\n[[token]]\nname = \"A\"\nvalue = \"w\"",
            "login = 3",
        ] {
            assert!(Vault::parse(bad).is_err(), "{bad}");
        }
        let broken = Vault::parse("[[token]]\nname = \"A\"\nvalue = \"s3cret\" x\n");
        let said = format!("{:#}", broken.err().unwrap());
        assert_eq!(said, "vault.toml is not valid TOML (line 3)");
    }

    #[test]
    fn names_follow_their_patterns() {
        assert!(login_name("github") && login_name("my-bank_2") && login_name("0x"));
        assert!(!login_name("") && !login_name("-x") && !login_name("Git") && !login_name("a b"));
        assert!(!login_name(&"a".repeat(65)));
        assert!(token_name("GH_TOKEN") && token_name("_X1"));
        assert!(!token_name("1X") && !token_name("gh") && !token_name("A-B"));
        assert!(program_name("gh") && program_name("python3.12") && program_name("g++"));
        assert!(!program_name("$X") && !program_name("-c") && !program_name("a b"));
    }

    #[test]
    fn origins_must_match_exactly() {
        let site = "https://github.com";
        assert!(same_origin(site, "https://github.com/login?x=1"));
        assert!(same_origin(site, "https://github.com:443/"));
        for page in [
            "https://github.com.evil.io/login",
            "http://github.com/login",
            "https://gist.github.com/",
            "https://github.com:8443/",
            "javascript:alert(1)",
            "about:blank",
            "",
        ] {
            assert!(!same_origin(site, page), "{page}");
        }
        assert_eq!(
            page_address("✓ https://github.com/login\n"),
            Some("https://github.com/login")
        );
    }

    #[test]
    fn totp_matches_the_rfc_6238_vectors() {
        // RFC 6238 appendix B, SHA1, seed "12345678901234567890".
        let key = b"12345678901234567890";
        for (t, code) in [
            (59, "94287082"),
            (1111111109, "07081804"),
            (1111111111, "14050471"),
            (1234567890, "89005924"),
            (2000000000, "69279037"),
            (20000000000, "65353130"),
        ] {
            assert_eq!(hotp(key, t, 8), code, "{t}");
        }
        let seed = data_encoding::BASE32.encode(key);
        assert_eq!(totp(&seed, 59).unwrap(), "287082");
        // Lowercase, spaced and unpadded seeds are the same seed.
        assert_eq!(
            totp("gezd gnbv gy3t qojq gezd gnbv gy3t qojq", 59).unwrap(),
            "287082"
        );
        assert_eq!(totp("JBSWY3DPEHPK3PXP", 0).unwrap().len(), 6);
        assert!(totp("not base32!", 0).is_err());
    }

    #[test]
    fn scrub_replaces_values_longest_first() {
        let secrets = vec![
            ("A".to_string(), "abcd".to_string()),
            ("B".to_string(), "abcdefgh".to_string()),
            ("C".to_string(), "xy".to_string()),
        ];
        assert_eq!(
            scrub("abcdefgh and abcd and xy", &secrets),
            "[vault:B] and [vault:A] and xy"
        );
    }

    #[test]
    fn scrub_masks_encoded_and_partial_prints() {
        use base64::engine::general_purpose::{STANDARD, URL_SAFE};
        use base64::Engine;
        let token = "ghp_s3cret?Token>42";
        let secrets = vec![("GH_TOKEN".to_string(), token.to_string())];
        for user in ["x", "ab", "abc", ""] {
            for b64 in [
                STANDARD.encode(format!("{user}:{token}")),
                URL_SAFE.encode(token),
            ] {
                let out = scrub(&format!("> Authorization: Basic {b64}\n"), &secrets);
                assert!(out.contains("[vault:GH_TOKEN]"), "{out}");
                // What is left decodes to no more than a few bits of the token.
                let rest = out.replace("[vault:GH_TOKEN]", "");
                assert!(!rest.contains(&b64[b64.len() / 2..]), "{out}");
            }
        }
        // A print cut off partway, or a piece of the value, is masked too.
        let out = scrub("token=ghp_s3cre (truncated)", &secrets);
        assert_eq!(out, "token=[redacted] (truncated)");
        assert_eq!(scrub("cret?Token>", &secrets), "[redacted]");
        assert_eq!(scrub("ghp_ and Token", &secrets), "ghp_ and Token");
    }

    #[test]
    fn a_fill_stops_once_the_page_leaves_the_site() {
        let site = "https://github.com";
        assert_eq!(
            moved(site, Some("https://github.com/session"), &["username"]),
            None
        );
        let stop = moved(site, Some("https://evil.io/x"), &["username"]).unwrap();
        assert!(
            stop.starts_with("stopped: after the username was typed"),
            "{stop}"
        );
        assert!(stop.contains("on https://evil.io") && stop.contains("Submit nothing"));
        let stop = moved(site, None, &["username", "password"]).unwrap();
        assert!(stop.contains("username and password") && stop.contains("an unknown page"));
    }

    #[test]
    fn scrubbed_withholds_output_when_the_vault_cannot_be_read() {
        let dir = crate::test_dir("vault-broken");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {dir:?}\n")).unwrap();
        std::fs::write(
            path(&cfg),
            "[[token]]\nname = \"A\"\nvalue = \"s3cret\" x\n",
        )
        .unwrap();
        let out = scrubbed(&cfg, "the value is s3cret".into());
        assert_eq!(out, "[output withheld: the vault could not be read]");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn put_keeps_comments_secrets_and_grants_and_never_grants_itself() {
        let mut v = Vault::parse(SAMPLE).unwrap();
        // An edit with empty secrets keeps them, and the grant, on the same site.
        let edit = Entry {
            name: "github".into(),
            site: "https://github.com/".into(),
            username: "aiden2".into(),
            ..Entry::default()
        };
        assert!(!v.put(edit.clone()).unwrap());
        let e = v.get("github").unwrap();
        assert_eq!(
            (e.username.as_str(), e.password.as_str()),
            ("aiden2", "hunter22")
        );
        assert!(e.granted && !e.totp.is_empty());
        // A new site drops the grant.
        v.put(Entry {
            site: "https://gitlab.com".into(),
            ..edit
        })
        .unwrap();
        assert!(!v.get("github").unwrap().granted);
        // A new entry starts with no grants, whatever it carries.
        let added = v
            .put(Entry {
                kind: Kind::Token,
                name: "MAIL_PASSWORD".into(),
                value: "mailpass".into(),
                programs: vec!["curl".into()],
                ..Entry::default()
            })
            .unwrap();
        assert!(added);
        assert!(v.get("MAIL_PASSWORD").unwrap().programs.is_empty());
        let text = v.to_string();
        assert!(
            text.contains("# my logins") && text.contains("# fill only here"),
            "{text}"
        );
        assert!(Vault::parse(&text).is_ok());

        v.grant(Kind::Token, "MAIL_PASSWORD", &["himalaya", "printenv"])
            .unwrap();
        v.grant(Kind::Token, "MAIL_PASSWORD", &["himalaya"])
            .unwrap();
        assert_eq!(
            v.get("MAIL_PASSWORD").unwrap().programs,
            ["himalaya", "printenv"]
        );
        assert!(v.grant(Kind::Login, "MAIL_PASSWORD", &[]).is_err());
        assert!(v.grant(Kind::Token, "MAIL_PASSWORD", &["$X"]).is_err());
        v.revoke("MAIL_PASSWORD", Some("printenv")).unwrap();
        assert_eq!(v.get("MAIL_PASSWORD").unwrap().programs, ["himalaya"]);
        assert!(v.revoke("MAIL_PASSWORD", Some("printenv")).is_err());
        v.grant(Kind::Login, "github", &[]).unwrap();
        assert!(v.get("github").unwrap().granted);
        v.revoke("github", None).unwrap();
        assert!(!v.get("github").unwrap().granted);
        assert!(v.remove("github") && !v.remove("github"));
        assert!(v
            .put(Entry {
                kind: Kind::Login,
                name: "GH_TOKEN".into(),
                ..Entry::default()
            })
            .is_err());
    }

    #[test]
    fn owner_commands_edit_the_file_and_refuse_values() {
        let dir = crate::test_dir("vault-cmd");
        let path = dir.join(FILE);
        std::fs::write(&path, SAMPLE).unwrap();
        let said = |arg: &str| command(&path, arg, true).unwrap_or_else(|refused| refused);
        assert!(said("").contains("token GH_TOKEN"));
        // Typed in the web app, a grant is refused and nothing is granted; the rest is not.
        let web = command(&path, "grant token GH_TOKEN curl", false).unwrap_err();
        assert_eq!(web, crate::tools::NEEDS_PASSKEY);
        assert!(command(&path, "grant login github", false).is_err());
        assert!(command(&path, "", false).is_ok_and(|list| !list.contains("curl")));
        assert!(
            command(&path, "rm nosuch", true).is_err(),
            "a failure is an Err"
        );
        assert!(said("grant token GH_TOKEN jq").contains("gh, jq"));
        assert!(said("revoke GH_TOKEN").contains("no longer"));
        assert!(said("rm github").contains("removed"));
        assert!(said("rm github").contains("no entry"));
        assert!(said("add token X hunter2").contains("never go through a chat"));
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            !text.contains("github") && text.contains("programs = []"),
            "{text}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn fields_carry_the_values_and_a_fresh_code() {
        let login = Vault::parse(SAMPLE).unwrap().get("github").unwrap();
        let sel = |s: &str| Some(s.to_string());
        let f = fields(&login, &[sel("#u"), sel("#p"), sel("#otp")], 59).unwrap();
        assert_eq!(
            f,
            [
                ("username", "#u".to_string(), "aiden".to_string()),
                ("password", "#p".to_string(), "hunter22".to_string()),
                ("one-time code", "#otp".to_string(), "287082".to_string()),
            ]
        );
        assert!(fields(&login, &[None, sel("--eval"), None], 0).is_err());
        let dashed = Entry {
            password: "--json".into(),
            ..login.clone()
        };
        let err = fields(&dashed, &[None, sel("#p"), None], 0).unwrap_err();
        assert!(err.contains("password") && !err.contains("--json"), "{err}");
    }
}
