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
    /// The request headers a token is swapped in besides `Authorization`, lower case.
    pub headers: Vec<String>,
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
            .field("headers", &self.headers)
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
                for host in &self.hosts {
                    crate::broker::parse_host(host)
                        .map_err(|e| anyhow::anyhow!("token {name}: {e}"))?;
                }
                for header in &self.headers {
                    crate::broker::header_name(header)
                        .map_err(|e| anyhow::anyhow!("token {name}: {e}"))?;
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
            "hosts": self.hosts,
            "headers": self.headers,
            "url": self.url,
            "raw": self.raw,
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
            Kind::Token => {
                let programs = match self.programs.as_slice() {
                    [] => "not allowed for any program yet".to_string(),
                    p => format!("allowed for {}", p.join(", ")),
                };
                let hosts: Vec<&str> = self.hosts.iter().map(|h| shown(h)).collect();
                let sent = match (self.raw, hosts.is_empty()) {
                    (true, _) => "handed out as its real value (raw)".to_string(),
                    (false, true) => "bound to no host yet".into(),
                    (false, false) => format!("sent only to {}", hosts.join(", ")),
                };
                let url = if self.url && !self.raw {
                    ", also in URLs"
                } else {
                    ""
                };
                let headers = match (self.headers.is_empty(), self.raw) {
                    (false, false) => format!(", also in the headers {}", self.headers.join(", ")),
                    _ => String::new(),
                };
                format!("token {}: {programs}; {sent}{headers}{url}", self.name)
            }
        }
    }
}

/// A provider's credentials (DESIGN §4.25): a `[[service]]` table. Never a token: it has no
/// programs, no hosts and no surrogate, and only the daemon's own client reads it. `Debug`
/// shows the key's length.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Service {
    /// What it is for: one of [`SERVICES`].
    pub name: String,
    pub provider: String,
    /// The API secret; empty when the provider has none (Link's tokens are its own file).
    pub key: String,
    /// A provider's own id: a Link client id, a telephony connection id.
    pub account: String,
}

/// The services a vault may hold, one of each.
pub const SERVICES: [&str; 2] = ["cards", "calls"];

impl std::fmt::Debug for Service {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Service")
            .field("name", &self.name)
            .field("provider", &self.provider)
            .field("key", &self.key.len())
            .field("account", &self.account)
            .finish()
    }
}

impl Service {
    fn check(&self) -> anyhow::Result<()> {
        if !SERVICES.contains(&self.name.as_str()) {
            bail!("a service is named {}", SERVICES.join(" or "));
        }
        let plain = |s: &str| s.chars().all(|c| c.is_ascii_graphic());
        if self.provider.is_empty() || !login_name(&self.provider) {
            bail!("service {}: the provider is a lowercase name", self.name);
        }
        if !plain(&self.key) || !plain(&self.account) {
            bail!(
                "service {}: the key and the account take no spaces",
                self.name
            );
        }
        Ok(())
    }

    /// One line for `/vault` and `zephyr vault list`: never the key.
    pub fn line(&self) -> String {
        let key = if self.key.is_empty() {
            "no key"
        } else {
            "a key is saved"
        };
        format!("service {}: {}; {key}", self.name, self.provider)
    }
}

/// A binding as the owner reads it: without the port every HTTPS host has.
pub fn shown(host_port: &str) -> &str {
    host_port.strip_suffix(":443").unwrap_or(host_port)
}

/// A change to a token's binding, for `settings_log` (host names are not secrets).
#[derive(Debug, PartialEq, Eq)]
pub struct Change {
    /// `vault.NAME.hosts`, `.url` or `.raw`.
    pub key: String,
    pub old: String,
    pub new: String,
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
        for key in ["login", "token", "service"] {
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
        let mut seen = std::collections::HashSet::new();
        for s in v.services() {
            s.check().context("vault.toml")?;
            if !seen.insert(s.name.clone()) {
                bail!("vault.toml has two services named {}", s.name);
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

    /// The providers' credentials, `[[service]]`.
    pub fn services(&self) -> Vec<Service> {
        let tables = self.doc.get("service").and_then(Item::as_array_of_tables);
        let s = |t: &Table, k: &str| t.get(k).and_then(Item::as_str).unwrap_or("").to_string();
        (tables.into_iter().flat_map(|a| a.iter()))
            .map(|t| Service {
                name: s(t, "name"),
                provider: s(t, "provider"),
                key: s(t, "key"),
                account: s(t, "account"),
            })
            .collect()
    }

    pub fn service(&self, name: &str) -> Option<Service> {
        self.services().into_iter().find(|s| s.name == name)
    }

    /// Adds or replaces the service of that name; an empty key keeps the one saved for the
    /// same provider. Returns whether it is new.
    pub fn put_service(&mut self, mut new: Service) -> anyhow::Result<bool> {
        let old = self.service(&new.name);
        if let Some(old) = old.as_ref().filter(|o| o.provider == new.provider) {
            if new.key.is_empty() {
                new.key.clone_from(&old.key);
            }
        }
        new.check()?;
        let list = self
            .doc
            .entry("service")
            .or_insert(Item::ArrayOfTables(ArrayOfTables::new()))
            .as_array_of_tables_mut()
            .context("vault.toml")?;
        list.retain(|t| t.get("name").and_then(Item::as_str) != Some(new.name.as_str()));
        let mut t = Table::new();
        set(&mut t, "name", &new.name);
        set(&mut t, "provider", &new.provider);
        set(&mut t, "key", &new.key);
        set(&mut t, "account", &new.account);
        list.push(t);
        Ok(old.is_none())
    }

    pub fn remove_service(&mut self, name: &str) -> bool {
        let Some(list) = (self.doc.get_mut("service")).and_then(Item::as_array_of_tables_mut)
        else {
            return false;
        };
        let before = list.len();
        list.retain(|t| t.get("name").and_then(Item::as_str) != Some(name));
        list.len() < before
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
                // A changed value keeps its hosts; `url` and `raw` are `/vault`'s alone.
                if new.hosts.is_empty() {
                    new.hosts.clone_from(&old.hosts);
                }
                if new.headers.is_empty() {
                    new.headers.clone_from(&old.headers);
                }
                (new.url, new.raw) = (old.url, old.raw);
            }
            None => {
                new.granted = false;
                new.programs.clear();
                (new.url, new.raw) = (false, false);
            }
        }
        new.check()?;
        new.hosts = hosts_of(&new.hosts.iter().map(String::as_str).collect::<Vec<_>>())?;
        new.headers = headers_of(&new.headers.iter().map(String::as_str).collect::<Vec<_>>())?;
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
                set_hosts(t, &new.hosts);
                set_list(t, "headers", &new.headers);
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

    fn token(&self, name: &str) -> anyhow::Result<Entry> {
        let token = self.get(name).filter(|e| e.kind == Kind::Token);
        token.with_context(|| format!("no token named {name} in the vault"))
    }

    /// The owner binds a token to `hosts` (added to those it has): the only places the proxy
    /// swaps it in (DESIGN §4.21).
    pub fn bind(&mut self, name: &str, hosts: &[&str]) -> anyhow::Result<(String, Change)> {
        let e = self.token(name)?;
        let mut all = e.hosts.clone();
        let mut added = Vec::new();
        for host in hosts_of(hosts)? {
            if !all.contains(&host) {
                added.push(format!(
                    "{} is now decrypted by zephyr for every program the agent runs",
                    shown(&host)
                ));
                all.push(host);
            }
        }
        set_hosts(self.table(Kind::Token, name).expect("found above"), &all);
        let list: Vec<&str> = all.iter().map(|h| shown(h)).collect();
        let mut text = format!("{name} is now sent only to {}", list.join(", "));
        for line in added {
            text.push_str(&format!("\n{line}"));
        }
        let change = change(name, "hosts", e.hosts.join(", "), all.join(", "));
        Ok((text, change))
    }

    /// Takes a token's binding back: to one host, or to all of them.
    pub fn unbind(&mut self, name: &str, host: Option<&str>) -> anyhow::Result<(String, Change)> {
        let e = self.token(name)?;
        let left: Vec<String> = match host {
            None => Vec::new(),
            Some(host) => {
                let host = crate::broker::parse_host(host).map_err(anyhow::Error::msg)?;
                if !e.hosts.contains(&host) {
                    bail!("{name} was not bound to {}", shown(&host));
                }
                e.hosts.iter().filter(|h| **h != host).cloned().collect()
            }
        };
        set_hosts(self.table(Kind::Token, name).expect("found above"), &left);
        let text = match left.as_slice() {
            [] => format!("{name} is bound to no host now, so it cannot be used"),
            l => {
                let list: Vec<&str> = l.iter().map(|h| shown(h)).collect();
                format!("{name} is now sent only to {}", list.join(", "))
            }
        };
        let change = change(name, "hosts", e.hosts.join(", "), left.join(", "));
        Ok((text, change))
    }

    /// The owner names request headers a token is swapped in besides `Authorization` (added to
    /// those it has), or takes one back (all of them with `None`).
    pub fn header(
        &mut self,
        name: &str,
        add: &[&str],
        remove: Option<Option<&str>>,
    ) -> anyhow::Result<(String, Change)> {
        let e = self.token(name)?;
        let mut all = e.headers.clone();
        all.extend(headers_of(add)?);
        let mut unique: Vec<String> = Vec::new();
        for header in all {
            if !unique.contains(&header) {
                unique.push(header);
            }
        }
        match remove {
            Some(None) => unique.clear(),
            Some(Some(header)) => {
                let header = crate::broker::header_name(header).map_err(anyhow::Error::msg)?;
                if !unique.contains(&header) {
                    bail!("{name} was not swapped in the header {header}");
                }
                unique.retain(|h| *h != header);
            }
            None => {}
        }
        set_list(
            self.table(Kind::Token, name).expect("found above"),
            "headers",
            &unique,
        );
        let text = match unique.as_slice() {
            [] => format!("{name} is swapped in the Authorization header only"),
            list => format!(
                "{name} is swapped in the Authorization header and in {}",
                list.join(", ")
            ),
        };
        let change = change(name, "headers", e.headers.join(", "), unique.join(", "));
        Ok((text, change))
    }

    /// Sets a token's `url` (also swapped in the path and query) or `raw` (handed out as its
    /// real value).
    pub fn set_flag(
        &mut self,
        name: &str,
        flag: &str,
        on: bool,
    ) -> anyhow::Result<(String, Change)> {
        let e = self.token(name)?;
        let old = if flag == "raw" { e.raw } else { e.url };
        let t = self.table(Kind::Token, name).expect("found above");
        match on {
            true => set(t, flag, true),
            false => drop(t.remove(flag)),
        }
        let text = match (flag, on) {
            ("raw", true) => format!(
                "{name} is now handed to the agent's programs as its real value, which they can \
                 read and send anywhere"
            ),
            ("raw", false) => format!("{name} reaches the agent only as a stand-in again"),
            (_, true) => format!(
                "{name} is now also swapped in a request's path and query; a server may repeat \
                 its URL back"
            ),
            (_, false) => format!("{name} is swapped in headers only"),
        };
        Ok((text, change(name, flag, old.to_string(), on.to_string())))
    }

    /// Every value a command or a page could echo back, by entry name, for [`scrub`].
    pub fn secrets(&self) -> Vec<(String, String)> {
        self.entries()
            .into_iter()
            .map(|e| match e.kind {
                Kind::Login => (e.name, e.password),
                Kind::Token => (e.name, e.value),
            })
            .chain(self.services().into_iter().map(|s| (s.name, s.key)))
            .collect()
    }

    /// Names, kinds, sites, usernames and grants; never a value.
    pub fn list(&self) -> String {
        let mut lines: Vec<String> = self.entries().iter().map(Entry::line).collect();
        lines.extend(self.services().iter().map(Service::line));
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

/// `hosts` as bindings, each checked and once.
fn hosts_of(hosts: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut all = Vec::new();
    for host in hosts {
        let host = crate::broker::parse_host(host).map_err(anyhow::Error::msg)?;
        if !all.contains(&host) {
            all.push(host);
        }
    }
    Ok(all)
}

/// Writes a token's `hosts`; none leaves no key.
fn set_hosts(t: &mut Table, hosts: &[String]) {
    set_list(t, "hosts", hosts)
}

/// Writes a list of a token's; an empty one leaves no key.
fn set_list(t: &mut Table, key: &str, items: &[String]) {
    match items.is_empty() {
        true => drop(t.remove(key)),
        false => set(t, key, Array::from_iter(items.iter())),
    }
}

/// `headers` as a token keeps them, each checked and once. `Authorization` is every token's
/// already.
fn headers_of(headers: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut all = Vec::new();
    for header in headers {
        let header = crate::broker::header_name(header).map_err(anyhow::Error::msg)?;
        if header != "authorization" && !all.contains(&header) {
            all.push(header);
        }
    }
    Ok(all)
}

fn change(name: &str, what: &str, old: String, new: String) -> Change {
    Change {
        key: format!("vault.{name}.{what}"),
        old,
        new,
    }
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
        headers: list("headers"),
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
    let mut forms = forms(secrets);
    forms.sort_by_key(|(_, v)| std::cmp::Reverse(v.len()));
    let mut out = text.to_string();
    for (name, v) in &forms {
        if out.contains(v.as_str()) {
            out = out.replace(v.as_str(), &format!("[vault:{name}]"));
        }
    }
    let all: Vec<String> = forms.into_iter().map(|(_, v)| v).collect();
    // And the numbers of the cards in use, which no file holds (DESIGN §4.25).
    crate::cards::scrub(&crate::auth::redact(&out, &all))
}

/// Every text a value shows up as, with its entry's name: itself and its base64 forms.
fn forms(secrets: &[(String, String)]) -> Vec<(&str, String)> {
    let mut forms: Vec<(&str, String)> = Vec::new();
    for (name, v) in secrets.iter().filter(|(_, v)| v.len() >= MIN_SCRUB) {
        forms.push((name.as_str(), v.clone()));
        forms.extend(base64_forms(v).into_iter().map(|f| (name.as_str(), f)));
    }
    forms
}

/// Copies `input` to `log` with every value of the vault at `vault`, and its base64 forms,
/// replaced by `[vault:NAME]`: what a command prints, on its way to its log (DESIGN §4.13), so
/// the log the agent can read holds none. A value cut across two reads is still found; after a
/// quarter of a second with nothing more, what was held back as the possible start of one goes
/// out as it is. The vault is read again when its file changes, and while it cannot be read the
/// output is withheld, as `scrubbed` withholds it. `cards` is the daemon's list of the card
/// numbers in use (`cards::live::shared`), read again at each chunk: they become `[card:#N]`.
fn scrub_stream(
    vault: &Path,
    cards: Option<std::fs::File>,
    mut input: std::fs::File,
    mut log: impl std::io::Write,
) {
    use std::io::Read;
    use std::os::fd::AsRawFd;
    const PAUSE_MS: i32 = 250;
    let stamp = || {
        let m = std::fs::metadata(vault).ok()?;
        Some((m.modified().ok()?, m.len()))
    };
    let mut put = |bytes: Vec<u8>| {
        // A log that cannot be written (a full disk) drops output; the command runs on.
        let _ = log.write_all(&bytes).and_then(|()| log.flush());
    };
    // `None`: the vault could not be read.
    let mut replacer: Option<crate::broker::Replacer> = None;
    // A card's code and expiry are replaced only where they stand alone, which needs the
    // byte on either side: done on whole lines, after the replacer.
    let mut lines = Lines::default();
    // The cards' list as last read whole: one being rewritten is read again at the next
    // chunk, and the list in hand is kept meanwhile, never an empty one.
    let mut list = String::new();
    let (mut seen, mut withheld) = (None, false);
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        if replacer.as_ref().is_some_and(|r| r.holds()) || lines.holds() {
            let mut wait = libc::pollfd {
                fd: input.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            // SAFETY: one pollfd of ours, for the length given.
            if unsafe { libc::poll(&mut wait, 1, PAUSE_MS) } == 0 {
                let held = replacer.as_mut().map(|r| r.finish()).unwrap_or_default();
                put(lines.push(held));
                put(lines.finish());
                continue;
            }
        }
        let n = match input.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        };
        if let Some(now) = cards.as_ref().and_then(crate::cards::read_shared) {
            list = now;
        }
        let now = Some((stamp(), list.clone()));
        if now != seen {
            let held = replacer.as_mut().map(|r| r.finish()).unwrap_or_default();
            put(lines.push(held));
            put(lines.finish());
            replacer = Vault::load(vault).ok().map(|v| {
                let pair = |(name, form): (&str, String)| {
                    (form.into_bytes(), format!("[vault:{name}]").into_bytes())
                };
                let mut pairs: Vec<_> = forms(&v.secrets()).into_iter().map(pair).collect();
                pairs.extend(crate::cards::stream_pairs(&list));
                crate::broker::Replacer::new(pairs)
            });
            lines.tokens = crate::cards::stream_tokens(&list);
            seen = now;
        }
        match &mut replacer {
            Some(r) => {
                withheld = false;
                put(lines.push(r.push(&buf[..n])));
            }
            None if !withheld => {
                withheld = true;
                put(b"[output withheld: the vault could not be read]\n".to_vec());
            }
            None => {}
        }
    }
    let held = replacer.as_mut().map(|r| r.finish()).unwrap_or_default();
    put(lines.push(held));
    put(lines.finish());
}

/// The second pass of a command's log: a card's code and expiry, replaced where no digit
/// stands beside them (`cards::scrub_tokens`). It works on whole lines, holding back what
/// follows the last newline, so a token is never judged with half of its neighbours.
/// shortcut: what is held back goes out after the 250 ms pause as it is then, so a code cut
/// across that pause is judged in two halves; buffer by time if a slow printer ever shows one.
#[derive(Default)]
struct Lines {
    tokens: Vec<(Vec<u8>, Vec<u8>)>,
    held: Vec<u8>,
}

impl Lines {
    fn holds(&self) -> bool {
        !self.held.is_empty()
    }

    /// The whole lines `bytes` completes, scrubbed.
    fn push(&mut self, bytes: Vec<u8>) -> Vec<u8> {
        if self.tokens.is_empty() && self.held.is_empty() {
            return bytes;
        }
        self.held.extend(bytes);
        let Some(end) = self.held.iter().rposition(|b| *b == b'\n') else {
            return Vec::new();
        };
        let rest = self.held.split_off(end + 1);
        let whole = std::mem::replace(&mut self.held, rest);
        crate::cards::scrub_tokens(&whole, &self.tokens)
    }

    /// What was held back, scrubbed as it is.
    fn finish(&mut self) -> Vec<u8> {
        let held = std::mem::take(&mut self.held);
        crate::cards::scrub_tokens(&held, &self.tokens)
    }
}

/// The descriptor a scrubber finds the daemon's card numbers on, when it is given `cards`.
pub const CARDS_FD: i32 = 3;

/// `zephyr __scrublog VAULT [cards]`: `scrub_stream` from stdin to stdout. The daemon starts
/// it, as itself, between a command and its log (`procs::spawn`); it ends when the command,
/// and all it left running, have closed their output. With `cards`, descriptor 3 is the
/// daemon's list of card numbers.
pub fn scrub_log(vault: &Path, cards: bool) -> std::process::ExitCode {
    use std::os::fd::FromRawFd;
    // It holds the vault's values, as the daemon does.
    crate::agent::undumpable();
    // SAFETY: this process's own stdin and stdout, used by nothing else here.
    let (input, log) = unsafe { (std::fs::File::from_raw_fd(0), std::fs::File::from_raw_fd(1)) };
    // SAFETY: the daemon put the list there for this process alone (`procs::spawn`).
    let cards = cards.then(|| unsafe { std::fs::File::from_raw_fd(CARDS_FD) });
    scrub_stream(vault, cards, input, log);
    std::process::ExitCode::SUCCESS
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
        Ok(v) => scrub(&text, &crate::broker::secrets(cfg, &v)),
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
pub fn command(
    path: &Path,
    arg: &str,
    may_approve: bool,
    strong: bool,
) -> Result<(String, Option<Change>), String> {
    let words: Vec<&str> = arg.split_whitespace().collect();
    let edit = |f: &dyn Fn(&mut Vault) -> anyhow::Result<String>| match edit(path, f) {
        Ok(text) => Ok((format!("[zephyr] {text}"), None)),
        Err(e) => Err(format!("[zephyr] {e:#}")),
    };
    // A binding: its change goes to settings_log.
    type Bound<'a> = &'a dyn Fn(&mut Vault) -> anyhow::Result<(String, Change)>;
    let bound = |f: Bound| match self::edit(path, f) {
        Ok((text, change)) => Ok((format!("[zephyr] {text}"), Some(change))),
        Err(e) => Err(format!("[zephyr] {e:#}")),
    };
    match words[..] {
        [] | ["list"] => {
            (Vault::load(path).map(|v| (v.list(), None))).map_err(|e| format!("[zephyr] {e:#}"))
        }
        // In the web app every approval is on its card, signed with a passkey: a grant, and
        // whatever loosens where a token goes, whatever `strong_channels` says.
        ["grant", ..] if !may_approve => Err(crate::tools::NEEDS_PASSKEY.into()),
        _ if !may_approve && loosens(&format!("/vault {arg}")) => {
            Err(crate::tools::NEEDS_PASSKEY.into())
        }
        // Where a token goes, or handing out the value itself: from a strong channel only
        // (`strong`), as the connectors' `hosts` and `secrets` are (DESIGN §4.21).
        _ if !strong && loosens(&format!("/vault {arg}")) => Err(
            "[zephyr] sending a vault token to more hosts, in more headers, in URLs or as its real \
             value needs a strong channel"
                .into(),
        ),
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
        ["bind", name, ref hosts @ ..] if !hosts.is_empty() => bound(&|v| v.bind(name, hosts)),
        ["unbind", name] => bound(&|v| v.unbind(name, None)),
        ["unbind", name, host] => bound(&|v| v.unbind(name, Some(host))),
        ["header", name, ref headers @ ..] if !headers.is_empty() => {
            bound(&|v| v.header(name, headers, None))
        }
        ["unheader", name] => bound(&|v| v.header(name, &[], Some(None))),
        ["unheader", name, header] => bound(&|v| v.header(name, &[], Some(Some(header)))),
        [flag @ ("raw" | "url"), name, on @ ("on" | "off")] => {
            bound(&|v| v.set_flag(name, flag, on == "on"))
        }
        ["add" | "edit", ..] => Err(
            "[zephyr] values never go through a chat: in zephyr attach, \
            /vault opens a form; on the host, run `zephyr vault add login NAME --site URL` or \
            `zephyr vault add token NAME --host HOST`"
                .into(),
        ),
        _ => Err(
            "[zephyr] usage: /vault, /vault rm NAME, /vault revoke NAME [PROGRAM], \
              /vault grant login NAME, /vault grant token NAME PROGRAM.., \
              /vault bind NAME HOST.., /vault unbind NAME [HOST], /vault header NAME HEADER.., \
              /vault unheader NAME [HEADER], /vault url NAME on|off, \
              /vault raw NAME on|off"
                .into(),
        ),
    }
}

/// Whether the owner's command `text` loosens where a token goes: `/vault bind`, `/vault
/// header`, `/vault url NAME on` and `/vault raw NAME on`. Taking any of them back does not.
pub fn loosens(text: &str) -> bool {
    let words: Vec<&str> = text.split_whitespace().collect();
    matches!(
        words[..],
        ["/vault", "bind" | "header", ..] | ["/vault", "raw" | "url", _, "on"]
    )
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
            let value = value_for(cfg, &token)?;
            env.push((token.name, value));
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

/// What a command gets for a token it may use (DESIGN §4.21): under the network floor its
/// stand-in, which the proxy swaps on the token's hosts; the real value for a `raw` token, and
/// under host networking, where there is no proxy to swap anything. Never the real value in
/// place of a stand-in that cannot be made.
fn value_for(cfg: &Config, token: &Entry) -> Result<String, String> {
    if token.raw || !crate::broker::on(cfg) {
        return Ok(token.value.clone());
    }
    if token.hosts.is_empty() {
        return Err(format!(
            "not run: the token {} is bound to no host; ask the owner with vault bind",
            token.name
        ));
    }
    match crate::broker::key(cfg) {
        Ok(key) => Ok(crate::broker::surrogate(&key, "token", &token.name)),
        Err(_) => Err("not run: the token broker's key could not be read".into()),
    }
}

/// What a fill answers while the owner holds the desktop (DESIGN §4.11).
pub const HELD: &str = "not filled: the owner has the desktop; try again when they hand it back";

/// The agent's `vault` call `call_id` (main only), answered through the gateway file: at once,
/// except a fill, which runs the browser from a task of its own.
pub fn agent(
    cfg: &Arc<Config>,
    store: &Store,
    call_id: &str,
    action: VaultAction,
    desktop: &Arc<crate::desktop::Control>,
) {
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
        VaultAction::Bind { name, hosts } => answer(&propose_bind(store, &vault, &name, &hosts)),
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
            // A fill and a takeover never overlap: the owner's hands are on the page.
            let Some(filling) = desktop.fill() else {
                return answer(HELD);
            };
            let (cfg, id) = (cfg.clone(), call_id.to_string());
            tokio::spawn(async move {
                let text = fill(&cfg, &login, [user, password, totp]).await;
                drop(filling);
                crate::tools::answer_gateway(&cfg, &id, &text);
            });
        }
    }
}

/// The agent's `bind`: a proposal `/vault bind NAME HOST..` for the owner, hosts first. The
/// agent can ask; only the owner's `/ok`, on a channel that may approve, binds.
fn propose_bind(store: &Store, vault: &Vault, name: &str, hosts: &[String]) -> String {
    if vault.token(name).is_err() {
        return format!("no token named {name} in the vault; `request` it from the owner");
    }
    let hosts = match hosts_of(&hosts.iter().map(String::as_str).collect::<Vec<_>>()) {
        Ok(hosts) => hosts,
        Err(e) => return format!("not asked: {e:#}"),
    };
    let hosts: Vec<&str> = hosts.iter().map(|h| shown(h)).collect();
    let command = format!("/vault bind {name} {}", hosts.join(" "));
    let ask = format!("to send the token {name} to {}", hosts.join(", "));
    match crate::tools::ask_owner(store, &command, &ask, ALLOWS) {
        Ok(id) => format!(
            "not bound: sending {name} to {} needs the owner's OK, asked as #{id}. Tell the owner \
             and end your turn; use it once they allow it.",
            hosts.join(", ")
        ),
        Err(e) => format!("not bound: asking the owner failed: {e}"),
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
pub(crate) fn page_address(out: &str) -> Option<&str> {
    out.split_whitespace().find(|w| origin(w).is_some())
}

/// Where the browser's page is now, as `get url` says: `None` when that fails or is no http(s) page.
pub(crate) async fn page_now(cfg: &Config) -> Option<String> {
    let out = browser(cfg, &["get", "url"]).await.ok()?;
    page_address(&out).map(String::from)
}

/// Why a fill stops after typing `done` because the page left `site` (`now`), or `None` while the
/// page is still on it.
pub(crate) fn moved(site: &str, now: Option<&str>, done: &[&str]) -> Option<String> {
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

/// What typing one secret into the page came to.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Typed {
    Ok,
    /// The page was not on the site when the value was about to be typed: its origin then
    /// (a card: nothing was typed), or its address (a login, [`fill_login`]: the value may
    /// have reached it).
    Moved(String),
    /// No field in the page's own document matches the selector: it is not there, or it is
    /// inside a frame, which is never typed into (a frame is another site's document).
    Missing,
    /// What the selector names takes no text.
    NotField,
    Failed(String),
}

/// The page script that types one value (DESIGN §4.15, §4.25). It runs in the page's top
/// document, checks that document's origin itself, and only then sets the field: the check
/// and the typing are one step of the page, with no navigation between them. A field in a
/// frame is not in the top document, so it is never found.
fn type_script(origin: &str, selector: &str, value: &str) -> String {
    let json = |s: &str| serde_json::Value::String(s.to_string()).to_string();
    format!(
        "(() => {{\nconst want = {};\nconst sel = {};\nconst value = {};\n{TYPE_BODY}\n}})()",
        json(origin),
        json(selector),
        json(value)
    )
}

const TYPE_BODY: &str = r#"if (window.top !== window) return "frame";
if (location.origin !== want) return "moved:" + location.origin;
let el = null;
try { el = document.querySelector(sel); } catch (e) { return "missing"; }
if (!el) return "missing";
const proto = el instanceof HTMLInputElement ? HTMLInputElement.prototype
  : el instanceof HTMLTextAreaElement ? HTMLTextAreaElement.prototype
  : el instanceof HTMLSelectElement ? HTMLSelectElement.prototype : null;
if (!proto) return "notfield";
el.focus();
Object.getOwnPropertyDescriptor(proto, "value").set.call(el, value);
el.dispatchEvent(new Event("input", { bubbles: true }));
el.dispatchEvent(new Event("change", { bubbles: true }));
return location.origin === want ? "ok" : "moved:" + location.origin;"#;

/// Overwrites a secret's bytes before its buffer is freed: a copy the daemon made to hand a
/// value on is not left in freed memory.
pub(crate) fn wipe(bytes: &mut [u8]) {
    for b in bytes.iter_mut() {
        // SAFETY: a valid, exclusive reference to one byte; the volatile write is not elided.
        unsafe { std::ptr::write_volatile(b, 0) };
    }
    std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
}

/// [`wipe`] for a string: zero bytes are valid UTF-8.
pub(crate) fn wipe_str(s: &mut str) {
    // SAFETY: every byte becomes 0, which is valid UTF-8.
    wipe(unsafe { s.as_bytes_mut() });
}

/// Types `value` into the field `selector` of the page open in agent-browser, if that page's
/// top document is on `site`'s origin (`Typed`).
///
/// The value never is an argument of any process: `/proc/<pid>/cmdline` is readable by every
/// uid, the agent's included. It goes to agent-browser 0.41 on the standard input of its
/// `batch` command (a JSON array of commands), which first goes back to the main frame and
/// then runs [`type_script`]; `--omit-command` keeps the commands out of what it prints.
pub(crate) async fn type_into(cfg: &Config, site: &str, selector: &str, value: &str) -> Typed {
    let Some(want) = origin(site).map(|o| o.ascii_serialization()) else {
        return Typed::Failed("the site is no http(s) address".into());
    };
    let mut script = type_script(&want, selector, value);
    let steps = batch(cfg, &(("frame", "main"), ("eval", &script)), value).await;
    wipe_str(&mut script);
    let steps = match steps {
        Ok(steps) => steps,
        Err(e) => return Typed::Failed(e),
    };
    let typed = said(&steps, 0).and_then(|_| said(&steps, 1));
    match typed.map(|r| r["result"].as_str().unwrap_or_default().to_string()) {
        Ok(r) if r == "ok" => Typed::Ok,
        Ok(r) if r == "missing" || r == "frame" => Typed::Missing,
        Ok(r) if r == "notfield" => Typed::NotField,
        Ok(r) => match r.strip_prefix("moved:") {
            Some(on) => Typed::Moved(on.chars().take(200).collect()),
            None => Typed::Failed("agent-browser gave no answer".into()),
        },
        Err(e) => Typed::Failed(e.into()),
    }
}

/// Types a login's `value` into `selector` with agent-browser's own `fill`, which takes its
/// refs (`@e2`), Playwright selectors and fields in a shadow root, as [`type_into`] does: on
/// standard input, `[["get","url"],["fill",SELECTOR,VALUE]]`. The address read right before
/// the fill, in the same batch, is the origin check; off the site it is `Moved`, its address.
async fn fill_login(cfg: &Config, site: &str, selector: &str, value: &str) -> Typed {
    let steps = match batch(cfg, &(("get", "url"), ("fill", selector, value)), value).await {
        Ok(steps) => steps,
        Err(e) => return Typed::Failed(e),
    };
    let url = match said(&steps, 0) {
        Ok(r) => r["url"].as_str().unwrap_or_default(),
        Err(e) => return Typed::Failed(e.into()),
    };
    if !same_origin(site, url) {
        return Typed::Moved(url.into());
    }
    match said(&steps, 1) {
        Ok(_) => Typed::Ok,
        Err(e) => Typed::Failed(e.into()),
    }
}

/// Runs `commands` (which hold `value`) as one agent-browser 0.41 `batch` on its standard
/// input, stopping at a failed step (`--bail`); `--omit-command` keeps the commands out of
/// what it prints. Its steps' answers, or why it did not run, the value replaced.
async fn batch(cfg: &Config, commands: &impl Serialize, value: &str) -> Result<Vec<Value>, String> {
    let mut input = serde_json::to_vec(commands).map_err(|e| e.to_string())?;
    let args = ["batch", "--json", "--omit-command", "--bail"];
    let out = browser_with(cfg, &args, Some(&input)).await;
    wipe(&mut input);
    let out = out.map_err(|e| e.replace(value, "[the value]"))?;
    Ok(serde_json::from_str(out.trim()).unwrap_or_default())
}

/// Step `i`'s result. A failed step's error is the browser's or the page's to word, so the
/// agent is told only that it failed.
fn said(steps: &[Value], i: usize) -> Result<&Value, &'static str> {
    let step = steps.get(i).ok_or("agent-browser gave no answer")?;
    match step["success"].as_bool() {
        Some(true) => Ok(&step["result"]),
        _ => Err("the page refused the value"),
    }
}

/// Why a field was not typed, for the agent: `None` when it was.
pub(crate) fn not_typed(typed: &Typed, label: &str) -> Option<String> {
    Some(match typed {
        Typed::Ok => return None,
        Typed::Moved(on) => format!("the page was on {on} when the {label} was to be typed"),
        Typed::Missing => format!(
            "no field for the {label} matches that selector in the page's own document; a \
             field inside a frame (an embedded form from another site) is never filled"
        ),
        Typed::NotField => format!("what the selector for the {label} names takes no text"),
        Typed::Failed(e) => e.clone(),
    })
}

/// Types the login's fields into the page open in agent-browser, once the page is on its site.
/// Each value is typed by agent-browser's own `fill` right after the page's address is read,
/// in one batch (`fill_login`); the address is read again after each field, so a page that
/// moves away gets no further field.
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
    if let Err(why) = browser_daemon(cfg) {
        return format!("not filled: {why}");
    }
    let mut done: Vec<&str> = Vec::new();
    for (label, sel, value) in &fields {
        // On standard input, never an argument: any uid can read a process's arguments.
        let typed = fill_login(cfg, &login.site, sel, value).await;
        if let Typed::Moved(now) = &typed {
            done.push(label);
            return moved(&login.site, Some(now), &done).unwrap_or_default();
        }
        if let Some(e) = not_typed(&typed, label) {
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

/// The socket of the agent-browser daemon the daemon's own fills talk to, as 0.41 finds it
/// (`connection.rs` `get_socket_dir`) in the environment `browser` hands it:
/// `AGENT_BROWSER_SOCKET_DIR`, else `XDG_RUNTIME_DIR/agent-browser`, else the agent's
/// `~/.agent-browser`; session `default`. shortcut: `AGENT_BROWSER_NAMESPACE` is not followed;
/// the daemon never sets it.
pub fn browser_socket(cfg: &Config) -> PathBuf {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    let dir = (var("AGENT_BROWSER_SOCKET_DIR").map(PathBuf::from))
        .or_else(|| var("XDG_RUNTIME_DIR").map(|d| PathBuf::from(d).join("agent-browser")))
        .unwrap_or_else(|| cfg.home.join(".agent-browser"));
    let session = var("AGENT_BROWSER_SESSION").unwrap_or_else(|| "default".into());
    dir.join(format!("{}.sock", session.to_string_lossy()))
}

/// Whether the program listening at `socket` is `real`, the real agent-browser: that socket is
/// in the agent's home, so the agent can put a program of its own there that reads every value
/// typed. `Ok` when nothing listens (the fill's first command starts the real one). Not seen:
/// the real binary started by the agent with options of its own (another Chromium, a CDP
/// endpoint), and a page's own scripts; DESIGN §4.25 says what bounds that.
pub(crate) fn browser_daemon_is(socket: &Path, real: &Path) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let Ok(conn) = std::os::unix::net::UnixStream::connect(socket) else {
        return Ok(());
    };
    let mut cred = libc::ucred {
        pid: 0,
        uid: 0,
        gid: 0,
    };
    let mut len = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
    // SAFETY: an open socket, and a buffer of the size given.
    let got = unsafe {
        libc::getsockopt(
            conn.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            (&mut cred as *mut libc::ucred).cast(),
            &mut len,
        )
    };
    let exe = (got == 0)
        .then(|| std::fs::read_link(format!("/proc/{}/exe", cred.pid)).ok())
        .flatten();
    let real = std::fs::canonicalize(real).unwrap_or_else(|_| real.to_path_buf());
    match exe {
        Some(exe) if exe == real => Ok(()),
        exe => Err(format!(
            "the browser's socket {} is held by {} (pid {}), which is not agent-browser; \
             nothing secret is typed into the browser until that program is gone",
            socket.display(),
            exe.map_or("a program zephyr cannot see".into(), |e| e
                .display()
                .to_string()),
            cred.pid
        )),
    }
}

/// [`browser_daemon_is`] for the daemon's own fills.
pub fn browser_daemon(cfg: &Config) -> Result<(), String> {
    browser_daemon_is(&browser_socket(cfg), &cfg.agent_browser)
}

/// `agent-browser ARGS` as the agent, as `exec` runs it: its stdout, or why it failed. The real
/// binary, by full path (`cfg.agent_browser`): the daemon's own fill never goes through the
/// approvals gate's wrapper that `agent-browser` on the agent's PATH is (approvals spec §4.4).
pub(crate) async fn browser(cfg: &Config, args: &[&str]) -> Result<String, String> {
    browser_with(cfg, args, None).await
}

/// [`browser`], with `input` written to the command's standard input: how a secret reaches
/// agent-browser without being an argument.
async fn browser_with(cfg: &Config, args: &[&str], input: Option<&[u8]>) -> Result<String, String> {
    use tokio::io::AsyncWriteExt;
    let mut cmd = tokio::process::Command::new(&cfg.agent_browser);
    cmd.args(args)
        .current_dir(&cfg.home)
        .env("HOME", &cfg.home)
        .stdin(match input {
            Some(_) => std::process::Stdio::piped(),
            None => std::process::Stdio::null(),
        })
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    crate::procs::for_agent(cmd.as_std_mut());
    let run = async {
        let mut child = cmd.spawn()?;
        if let (Some(input), Some(mut stdin)) = (input, child.stdin.take()) {
            stdin.write_all(input).await?;
            stdin.shutdown().await?;
        }
        child.wait_with_output().await
    };
    let out = tokio::time::timeout(BROWSER_WAIT, run)
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

    const SERVICE: &str = "[[service]]\nname = \"cards\"\nprovider = \"link\"\n\
                           key = \"sk_servicekey42\"\naccount = \"lwlpk_x\"\n";

    #[test]
    fn a_service_is_listed_by_name_and_provider_only() {
        let v = Vault::parse(SERVICE).unwrap();
        let s = v.service("cards").unwrap();
        assert_eq!(
            (s.provider.as_str(), s.account.as_str()),
            ("link", "lwlpk_x")
        );
        assert_eq!(
            v.list(),
            "[zephyr] vault:\nservice cards: link; a key is saved"
        );
        assert!(!format!("{s:?}{}", v.list()).contains("sk_servicekey42"));
        // Its key is scrubbed like any vault value.
        assert_eq!(
            scrub("x sk_servicekey42 y", &v.secrets()),
            "x [vault:cards] y"
        );
        for bad in [
            "[[service]]\nname = \"other\"\nprovider = \"link\"\n",
            "[[service]]\nname = \"cards\"\n",
            &format!("{SERVICE}{SERVICE}"),
            "[service]\nname = \"cards\"\n",
        ] {
            assert!(Vault::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn a_service_is_never_a_token() {
        let mut v = Vault::parse(SERVICE).unwrap();
        assert!(
            v.get("cards").is_none(),
            "not an entry the agent's tools see"
        );
        assert!(v.grant(Kind::Token, "cards", &["curl"]).is_err());
        assert!(v.bind("cards", &["api.link.com"]).is_err());
        let dir = crate::test_dir("vault-service");
        let path = dir.join("vault.toml");
        std::fs::write(&path, SERVICE).unwrap();
        // A chat can neither add nor change one: `/vault` has no word for it.
        let said = command(&path, "add service cards", true, true);
        assert!(said.is_err(), "{said:?}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), SERVICE);
        // A new key for the same provider replaces it; an empty one keeps it.
        let again = Service {
            name: "cards".into(),
            provider: "link".into(),
            ..Service::default()
        };
        assert!(!v.put_service(again).unwrap());
        assert_eq!(v.service("cards").unwrap().key, "sk_servicekey42");
        assert!(v.remove_service("cards") && v.service("cards").is_none());
    }

    #[test]
    fn the_typing_script_checks_the_origin_and_the_frame_before_it_types() {
        let script = type_script("https://shop.example", "#n\"; alert(1); \"", "4242 \"x\"");
        // The selector and the value are JSON literals: neither can end the statement.
        assert!(
            script.contains(r##"const sel = "#n\"; alert(1); \"";"##),
            "{script}"
        );
        assert!(
            script.contains(r##"const value = "4242 \"x\"";"##),
            "{script}"
        );
        let (frame, origin, set) = (
            script.find("window.top !== window").unwrap(),
            script.find("location.origin !== want").unwrap(),
            script.find(".set.call(el, value)").unwrap(),
        );
        assert!(frame < origin && origin < set, "checked before it is typed");
        let mut secret = String::from("4242424242424242");
        wipe_str(&mut secret);
        assert_eq!(secret, "\0".repeat(16));
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
        let said =
            |arg: &str| command(&path, arg, true, true).map_or_else(|refused| refused, |d| d.0);
        assert!(said("").contains("token GH_TOKEN"));
        // Typed in the web app, a grant is refused and nothing is granted; the rest is not.
        let web = command(&path, "grant token GH_TOKEN curl", false, true).unwrap_err();
        assert_eq!(web, crate::tools::NEEDS_PASSKEY);
        assert!(command(&path, "grant login github", false, true).is_err());
        assert!(command(&path, "", false, false).is_ok_and(|list| !list.0.contains("curl")));
        assert!(
            command(&path, "rm nosuch", true, true).is_err(),
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

    fn brokered(name: &str, vault: &str) -> (Config, Store) {
        let dir = crate::test_dir(name);
        std::fs::write(dir.join(FILE), vault).unwrap();
        let mut cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {dir:?}\n")).unwrap();
        cfg.floor = true;
        let store = Store::open(&cfg.db_path()).unwrap();
        (cfg, store)
    }

    const TOKENS: &str = "[[token]]\nname = \"BOUND\"\nvalue = \"real-bound-1\"\nprograms = [\"curl\"]\nhosts = [\"api.x.test:443\"]\n\
        [[token]]\nname = \"RAW\"\nvalue = \"real-raw-12\"\nprograms = [\"curl\"]\nraw = true\n\
        [[token]]\nname = \"LOOSE\"\nvalue = \"real-loose-1\"\nprograms = [\"curl\"]\n";

    #[test]
    fn exec_env_gives_a_surrogate_a_raw_value_or_nothing() {
        let (mut cfg, store) = brokered("vault-env", TOKENS);
        let env = |cfg: &Config, name: &str| exec_env(cfg, &store, "curl x", &[name.to_string()]);
        let key = crate::broker::key(&cfg).unwrap();
        let surrogate = crate::broker::surrogate(&key, "token", "BOUND");
        assert_eq!(
            env(&cfg, "BOUND").unwrap(),
            [("BOUND".to_string(), surrogate)]
        );
        assert_eq!(env(&cfg, "RAW").unwrap()[0].1, "real-raw-12");
        assert_eq!(
            env(&cfg, "LOOSE").unwrap_err(),
            "not run: the token LOOSE is bound to no host; ask the owner with vault bind"
        );
        // A key that cannot be read gives no value in its place.
        std::fs::write(cfg.state_dir().join(crate::broker::KEY_FILE), "broken").unwrap();
        let refused = env(&cfg, "BOUND").unwrap_err();
        assert!(
            refused.starts_with("not run: ") && !refused.contains("real-"),
            "{refused}"
        );
        // Under host networking nothing can swap: every token is handed out as before.
        cfg.floor = false;
        assert_eq!(env(&cfg, "BOUND").unwrap()[0].1, "real-bound-1");
        assert_eq!(env(&cfg, "LOOSE").unwrap()[0].1, "real-loose-1");
    }

    #[test]
    fn put_keeps_hosts_across_a_new_value() {
        let mut v = Vault::parse(TOKENS).unwrap();
        v.set_flag("BOUND", "url", true).unwrap();
        let put = |v: &mut Vault, value: &str, hosts: &[&str], raw: bool| {
            v.put(Entry {
                kind: Kind::Token,
                name: "BOUND".into(),
                value: value.into(),
                hosts: hosts.iter().map(|h| h.to_string()).collect(),
                raw,
                ..Entry::default()
            })
        };
        // A new value keeps the hosts; a form never sets `raw` or `url`.
        put(&mut v, "rotated-value", &[], true).unwrap();
        let e = v.get("BOUND").unwrap();
        assert_eq!(
            (e.value.as_str(), e.url, e.raw),
            ("rotated-value", true, false)
        );
        assert_eq!(e.hosts, ["api.x.test:443"]);
        // The owner's own form replaces them, normalized; a wildcard saves nothing.
        put(
            &mut v,
            "",
            &["API.Y.test", "y.test:8443", "api.y.test:443"],
            false,
        )
        .unwrap();
        assert_eq!(
            v.get("BOUND").unwrap().hosts,
            ["api.y.test:443", "y.test:8443"]
        );
        assert!(put(&mut v, "", &["*.y.test"], false).is_err());
        assert_eq!(v.get("BOUND").unwrap().hosts.len(), 2);
        // A new entry starts with what its form said, and neither flag.
        v.put(Entry {
            kind: Kind::Token,
            name: "NEW".into(),
            value: "new-value".into(),
            hosts: vec!["n.test".into()],
            url: true,
            raw: true,
            ..Entry::default()
        })
        .unwrap();
        let e = v.get("NEW").unwrap();
        assert_eq!(
            (e.hosts, e.url, e.raw),
            (vec!["n.test:443".to_string()], false, false)
        );
        assert!(
            Vault::parse("[[token]]\nname = \"X\"\nvalue = \"v\"\nhosts = [\"1.2.3.4\"]\n")
                .is_err()
        );
    }

    #[test]
    fn bind_raw_and_url_need_a_strong_channel_and_tightening_does_not() {
        let dir = crate::test_dir("vault-bind");
        let path = dir.join(FILE);
        std::fs::write(&path, TOKENS).unwrap();
        // On a channel that is not strong (Discord, say), whatever else it may approve.
        let weak = |arg: &str| command(&path, arg, true, false);
        let strong = |arg: &str| command(&path, arg, true, true);
        for loosening in ["bind LOOSE api.x.test", "raw LOOSE on", "url LOOSE on"] {
            let refused = weak(loosening).unwrap_err();
            assert!(
                refused.ends_with("needs a strong channel"),
                "{loosening}: {refused}"
            );
            assert!(loosens(&format!("/vault {loosening}")), "{loosening}");
        }
        // In the web app each of them is approved on its card with a passkey, whatever
        // `strong_channels` says: typed there, it is refused even when `web` is a strong one.
        for loosening in [
            "bind LOOSE api.x.test",
            "raw LOOSE on",
            "url LOOSE on",
            "header LOOSE X-Api-Key",
            "grant token LOOSE gh",
        ] {
            let refused = command(&path, loosening, false, true).unwrap_err();
            assert_eq!(refused, crate::tools::NEEDS_PASSKEY, "{loosening}");
        }
        assert!(command(&path, "unbind BOUND", false, false).is_ok());
        std::fs::write(&path, TOKENS).unwrap();
        // The headers a token is swapped in besides Authorization are the owner's to list.
        assert!(weak("header LOOSE X-Api-Key")
            .unwrap_err()
            .ends_with("needs a strong channel"));
        let (said, change) = strong("header LOOSE X-Api-Key x-goog-api-key Authorization").unwrap();
        assert_eq!(
            said,
            "[zephyr] LOOSE is swapped in the Authorization header and in x-api-key, x-goog-api-key"
        );
        assert_eq!(change.unwrap().key, "vault.LOOSE.headers");
        assert!(strong("header LOOSE bad:name").is_err());
        let list = weak("").unwrap().0;
        assert!(
            list.contains("also in the headers x-api-key, x-goog-api-key"),
            "{list}"
        );
        let (said, _) = weak("unheader LOOSE x-api-key").unwrap();
        assert!(said.ends_with("and in x-goog-api-key"), "{said}");
        assert!(weak("unheader LOOSE x-api-key").is_err());
        let (said, _) = weak("unheader LOOSE").unwrap();
        assert_eq!(
            said,
            "[zephyr] LOOSE is swapped in the Authorization header only"
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), TOKENS);
        let (said, change) = strong("bind LOOSE API.x.test gist.x.test:8443").unwrap();
        assert_eq!(
            said,
            "[zephyr] LOOSE is now sent only to api.x.test, gist.x.test:8443\n\
             api.x.test is now decrypted by zephyr for every program the agent runs\n\
             gist.x.test:8443 is now decrypted by zephyr for every program the agent runs"
        );
        let change = change.unwrap();
        assert_eq!(
            (
                change.key.as_str(),
                change.old.as_str(),
                change.new.as_str()
            ),
            ("vault.LOOSE.hosts", "", "api.x.test:443, gist.x.test:8443")
        );
        for bad in [
            "bind LOOSE *.x.test",
            "bind LOOSE https://x.test",
            "bind LOOSE 1.2.3.4",
            "bind NOPE x.test",
            "bind LOOSE",
        ] {
            assert!(strong(bad).is_err(), "{bad}");
        }
        assert!(strong("raw LOOSE on").unwrap().0.contains("real value"));
        assert!(strong("url LOOSE on").unwrap().1.is_some());
        let list = weak("").unwrap().0;
        assert!(
            list.contains("token LOOSE: allowed for curl; handed out as its real value (raw)"),
            "{list}"
        );
        assert!(
            list.contains("token BOUND: allowed for curl; sent only to api.x.test"),
            "{list}"
        );
        // Tightening is taken from any channel.
        for tightening in ["unbind LOOSE", "raw LOOSE off", "url LOOSE off"] {
            assert!(!loosens(&format!("/vault {tightening}")), "{tightening}");
        }
        let (_, change) = weak("raw LOOSE off").unwrap();
        assert_eq!(change.unwrap().key, "vault.LOOSE.raw");
        weak("url LOOSE off").unwrap();
        let (said, _) = weak("unbind LOOSE gist.x.test:8443").unwrap();
        assert_eq!(said, "[zephyr] LOOSE is now sent only to api.x.test");
        assert!(weak("unbind LOOSE gist.x.test:8443").is_err());
        let (said, _) = weak("unbind LOOSE").unwrap();
        assert!(said.contains("bound to no host"), "{said}");
        let e = Vault::load(&path).unwrap().get("LOOSE").unwrap();
        assert_eq!(
            (e.hosts.len(), e.url, e.raw, e.programs),
            (0, false, false, vec!["curl".to_string()])
        );
    }

    #[test]
    fn the_agents_bind_is_a_proposal_and_never_a_binding() {
        let (cfg, store) = brokered("vault-propose", TOKENS);
        let vault = Vault::load(&path(&cfg)).unwrap();
        let hosts = ["api.x.test".to_string(), "x.test".to_string()];
        let said = propose_bind(&store, &vault, "LOOSE", &hosts);
        assert!(
            said.starts_with(
                "not bound: sending LOOSE to api.x.test, x.test needs the owner's OK, asked as #1"
            ),
            "{said}"
        );
        assert_eq!(std::fs::read_to_string(path(&cfg)).unwrap(), TOKENS);
        assert!(propose_bind(&store, &vault, "NOPE", &hosts).contains("no token named NOPE"));
        assert!(
            propose_bind(&store, &vault, "LOOSE", &["*.x.test".to_string()])
                .starts_with("not asked")
        );
    }

    #[test]
    fn scrubbed_output_hides_a_surrogate() {
        let (cfg, _store) = brokered("vault-scrub-surrogate", TOKENS);
        let key = crate::broker::key(&cfg).unwrap();
        let surrogate = crate::broker::surrogate(&key, "token", "BOUND");
        let clean = scrubbed(&cfg, format!("BOUND={surrogate} and real-bound-1"));
        assert_eq!(clean, "BOUND=[vault:BOUND] and [vault:BOUND]");
        assert!(!format!("{:?}", Vault::load(&path(&cfg)).unwrap().get("BOUND")).contains("real-"));
    }

    #[test]
    fn a_commands_output_reaches_its_log_without_a_vault_value() {
        use base64::Engine;
        use std::io::Write;
        let dir = crate::test_dir("vault-scrub-stream");
        let vault = dir.join(FILE);
        std::fs::write(&vault, TOKENS).unwrap();
        let run = |writes: &[&[u8]], pause: Duration| -> String {
            let (rx, mut tx) = std::io::pipe().unwrap();
            let log = dir.join("out.log");
            let out = std::fs::File::create(&log).unwrap();
            let vault = vault.clone();
            let input = std::fs::File::from(std::os::fd::OwnedFd::from(rx));
            let cards = std::fs::File::open(dir.join("cards")).ok();
            let filter = std::thread::spawn(move || scrub_stream(&vault, cards, input, out));
            for part in writes {
                tx.write_all(part).unwrap();
                std::thread::sleep(pause);
            }
            drop(tx);
            filter.join().unwrap();
            std::fs::read_to_string(log).unwrap()
        };
        let basic = base64::engine::general_purpose::STANDARD.encode("x:real-bound-1");
        let text =
            format!("key=real-bound-1 raw=real-raw-12\nAuthorization: Basic {basic}\ndone\n");
        let clean = run(&[text.as_bytes()], Duration::ZERO);
        assert!(clean.starts_with("key=[vault:BOUND] raw=[vault:RAW]\nAuthorization: Basic "));
        assert!(
            clean.ends_with("\ndone\n") && clean.contains("Basic eDp[vault:BOUND]"),
            "{clean}"
        );
        assert!(
            !clean.contains("real-") && !clean.contains(&basic),
            "{clean}"
        );
        // Cut in the middle of a value, as a pipe may cut it.
        let clean = run(&[b"key=real-bo", b"und-1\n"], Duration::ZERO);
        assert_eq!(clean, "key=[vault:BOUND]\n");
        // A prompt that ends like the start of a value is not held for ever.
        let clean = run(&[b"so real-", b"ly\n"], Duration::from_millis(600));
        assert_eq!(clean, "so real-ly\n");
        // A card number in use, which no file of the vault's holds: read from the daemon's
        // list, again at each chunk, so a card issued while the command runs is covered.
        let clean = run(&[b"n=5555555555554444\n"], Duration::ZERO);
        assert_eq!(clean, "n=5555555555554444\n", "no list, nothing to find");
        let list = crate::cards::shared_bytes("7\t5555555555554444\t987\t11\t2030\n");
        std::fs::write(dir.join("cards"), &list).unwrap();
        let clean = run(
            &[
                b"n=55555555",
                b"55554444 or 5555 5555 5555 4444\nkey=real-bound-1\n",
            ],
            Duration::ZERO,
        );
        assert_eq!(clean, "n=[card:#7] or [card:#7]\nkey=[vault:BOUND]\n");
        // Its code and its expiry too, where they stand alone, cut across reads or not.
        let clean = run(
            &[
                b"exp 11/3",
                b"0 cvc 98",
                b"7 port 9870 on 2011/30\nlast 987",
            ],
            Duration::ZERO,
        );
        assert_eq!(
            clean,
            "exp [card:#7] cvc [card:#7] port 9870 on 2011/30\nlast [card:#7]"
        );
        // A list that is being rewritten (its sequence odd) is not read as an empty one: the
        // scrubber has none yet here, so it reads again at the next chunk.
        let mut torn = list.clone();
        torn[0] = 3;
        std::fs::write(dir.join("cards"), &torn).unwrap();
        let clean = run(&[b"n=5555555555554444\n"], Duration::ZERO);
        assert_eq!(clean, "n=5555555555554444\n", "no list was ever read whole");
        std::fs::write(dir.join("cards"), &list).unwrap();
        // A vault that cannot be read: nothing of the output is written.
        std::fs::write(&vault, "[[token]\nbroken").unwrap();
        let clean = run(&[b"key=real-bound-1\n", b"more\n"], Duration::ZERO);
        assert_eq!(clean, "[output withheld: the vault could not be read]\n");
    }

    /// The agent-browser daemon a fill would talk to is checked: nothing at its socket, or a
    /// stale socket, is fine (the fill starts the real one); a program that is not the real
    /// binary listening there is refused.
    #[test]
    fn a_browser_socket_held_by_another_program_is_refused() {
        let dir = crate::test_dir("vault-ab-socket");
        let socket = dir.join("default.sock");
        let me = std::env::current_exe().unwrap();
        let other = dir.join("agent-browser");
        assert_eq!(browser_daemon_is(&socket, &other), Ok(()), "none");
        drop(std::os::unix::net::UnixListener::bind(&socket).unwrap());
        assert_eq!(browser_daemon_is(&socket, &other), Ok(()), "stale");
        std::fs::remove_file(&socket).unwrap();
        let _held = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        // This test's own process holds it: not the real binary.
        let why = browser_daemon_is(&socket, &other).unwrap_err();
        assert!(why.contains("is not agent-browser"), "{why}");
        assert_eq!(browser_daemon_is(&socket, &me), Ok(()), "the real one");
    }
}
