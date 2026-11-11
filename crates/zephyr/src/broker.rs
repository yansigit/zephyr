//! The token broker (DESIGN §4.21): the agent holds a stand-in for each vault token and connector
//! secret, and the network floor's proxy puts the real value in only on the hosts the owner bound
//! it to. This file is the pure part: the stand-ins, the bindings, and what a request's head
//! becomes. Nothing here prints a value or a surrogate.

use crate::config::{Config, Mcp};
use crate::vault::{Kind, Vault};
use anyhow::Context;
use hyper::header::{HeaderMap, HeaderValue, AUTHORIZATION};
use hyper::Uri;
use std::collections::HashMap;

/// How every surrogate starts; 40 lowercase hex digits follow.
pub const PREFIX: &str = "zsur_";
const HEX: usize = 40;
/// `<state>/broker.key`: 32 random bytes as hex, root's, 0600.
pub const KEY_FILE: &str = "broker.key";

/// Whether tokens are brokered: only under the network floor, where the proxy can swap them.
pub fn on(cfg: &Config) -> bool {
    cfg.floor
}

fn read_key(path: &std::path::Path) -> anyhow::Result<Option<[u8; 32]>> {
    let text = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        r => r.context("reading broker.key")?,
    };
    let bytes = data_encoding::HEXLOWER.decode(text.trim().as_bytes());
    let key = bytes.ok().and_then(|b| <[u8; 32]>::try_from(b).ok());
    Ok(Some(key.context("broker.key is not a key")?))
}

/// The key surrogates are derived from, made on first use. Deleting the file and restarting the
/// container retires every surrogate at once.
pub fn key(cfg: &Config) -> anyhow::Result<[u8; 32]> {
    static MAKE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _held = MAKE.lock().unwrap_or_else(|e| e.into_inner());
    let path = cfg.state_dir().join(KEY_FILE);
    if let Some(key) = read_key(&path)? {
        return Ok(key);
    }
    let mut key = [0u8; 32];
    ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), &mut key)
        .map_err(|_| anyhow::anyhow!("no random bytes for broker.key"))?;
    crate::host::conf::replace_private(&path, &data_encoding::HEXLOWER.encode(&key))?;
    Ok(key)
}

/// The stand-in for the secret `name` of `kind` (`token`: a vault token, `env`: a secrets.env
/// name a connector uses). Derived, so it survives a re-exec and a changed value, and there is
/// no table to keep.
pub fn surrogate(key: &[u8; 32], kind: &str, name: &str) -> String {
    use ring::hmac;
    let mut ctx = hmac::Context::with_key(&hmac::Key::new(hmac::HMAC_SHA256, key));
    ctx.update(kind.as_bytes());
    ctx.update(&[0]);
    ctx.update(name.as_bytes());
    let tag = ctx.sign();
    format!(
        "{PREFIX}{}",
        data_encoding::HEXLOWER.encode(&tag.as_ref()[..HEX / 2])
    )
}

/// One binding as `host:port`: a name, exactly, on port 443 unless it names another. Never an
/// address, a wildcard, a scheme or a path: `*.github.io` would reach a page anyone can publish.
pub fn parse_host(entry: &str) -> Result<String, String> {
    let entry = entry.trim();
    let shape =
        || format!("{entry:?}: use HOST or HOST:PORT, a name with no wildcard, scheme or path");
    if entry.is_empty() || entry.contains(['*', '/']) {
        return Err(shape());
    }
    let authority = match entry.contains(':') {
        true => entry.to_string(),
        false => format!("{entry}:443"),
    };
    match crate::egress::target(&authority) {
        Ok((url::Host::Domain(name), port)) => Ok(format!("{name}:{port}")),
        _ => Err(shape()),
    }
}

/// `hosts` as a connector or the registry writes them: comma separated. Bad entries are left
/// out here; they were refused when the file loaded.
pub fn parse_hosts(raw: &str) -> Vec<String> {
    let entries = raw.split(',').filter(|h| !h.trim().is_empty());
    entries.filter_map(|h| parse_host(h).ok()).collect()
}

/// One brokered secret. `Debug` shows no value and no surrogate.
#[derive(Clone)]
pub struct Secret {
    pub name: String,
    value: String,
    surrogate: String,
    pub hosts: Vec<String>,
    pub url: bool,
}

impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Secret")
            .field("name", &self.name)
            .field("hosts", &self.hosts)
            .field("url", &self.url)
            .finish()
    }
}

/// Surrogate to secret, for every brokered token and connector secret. The values live here, in
/// the daemon's memory, and nowhere the agent can read.
#[derive(Default, Clone)]
pub struct Map {
    by: HashMap<String, Secret>,
    /// The base64 text a surrogate turns into (`vault::base64_forms`), with the surrogate.
    forms: Vec<(String, String)>,
    /// The vault could not be read: this is the last map, kept to say which hosts are
    /// decrypted, and nothing is swapped from it.
    pub unreadable: bool,
}

impl std::fmt::Debug for Map {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut names: Vec<&Secret> = self.by.values().collect();
        names.sort_by(|a, b| a.name.cmp(&b.name));
        f.debug_struct("Map")
            .field("secrets", &names)
            .field("unreadable", &self.unreadable)
            .finish()
    }
}

/// Every `${NAME}` in `text`.
fn names_in(text: &str, out: &mut Vec<String>) {
    let mut rest = text;
    while let Some(i) = rest.find("${") {
        let Some(len) = rest[i + 2..].find('}') else {
            break;
        };
        let name = &rest[i + 2..i + 2 + len];
        if !out.iter().any(|n| n == name) {
            out.push(name.to_string());
        }
        rest = &rest[i + 3 + len..];
    }
}

/// The secrets.env names a connector uses, anywhere `drafts::filled` fills them in, and those
/// its `url` names.
pub fn connector_names(m: &Mcp) -> (Vec<String>, Vec<String>) {
    let (mut all, mut in_url) = (Vec::new(), Vec::new());
    let texts = (m.url.iter().chain(&m.command).chain(&m.cwd))
        .chain(m.headers.values())
        .chain(&m.args)
        .chain(m.env.values());
    texts.for_each(|t| names_in(t, &mut all));
    m.url.iter().for_each(|t| names_in(t, &mut in_url));
    (all, in_url)
}

/// Where a connector's secrets go: its `hosts`, or its own `https` URL's host when it lists
/// none. Plain `http` is never swapped, so such a URL binds nothing.
pub fn connector_hosts(m: &Mcp) -> Vec<String> {
    let listed = parse_hosts(&m.hosts);
    if !listed.is_empty() {
        return listed;
    }
    let url = m.url.as_deref().and_then(|u| url::Url::parse(u).ok());
    let own = url.filter(|u| u.scheme() == "https").and_then(|u| {
        let port = u.port_or_known_default()?;
        parse_host(&format!("{}:{port}", u.host_str()?)).ok()
    });
    own.into_iter().collect()
}

/// Whether `m` names a secret and has nowhere to send it: a `command` connector, or one on
/// plain `http`, with no `hosts`. Such a connector is left out until the owner lists hosts or
/// says `secrets = "plain"`.
fn nowhere(m: &Mcp) -> bool {
    m.secrets.as_deref() != Some("plain")
        && !connector_names(m).0.is_empty()
        && connector_hosts(m).is_empty()
}

/// What `drafts::filled` fills `m`'s `${NAME}`s with, from secrets.env (`env`): surrogates
/// under the network floor, the real values for a `plain` connector and under host networking,
/// where no proxy could swap anything. `Err` says why the connector is left out; a real value
/// never stands in for a surrogate that cannot be made.
pub fn connector_secrets(
    cfg: &Config,
    m: &Mcp,
    env: &[(String, String)],
) -> Result<Vec<(String, String)>, String> {
    if !on(cfg) || m.secrets.as_deref() == Some("plain") || connector_names(m).0.is_empty() {
        return Ok(env.to_vec());
    }
    if nowhere(m) {
        return Err(format!(
            "it names a secret and no host to send it to: /set mcp.{0}.hosts HOST.., or /set \
             mcp.{0}.secrets plain to hand it the real one",
            m.name
        ));
    }
    let key = key(cfg).map_err(|_| "the token broker's key could not be read".to_string())?;
    let stand_in = |(name, _): &(String, String)| (name.clone(), surrogate(&key, "env", name));
    Ok(env.iter().map(stand_in).collect())
}

impl Map {
    fn insert(&mut self, key: &[u8; 32], kind: &str, name: &str, value: &str) -> &mut Secret {
        let surrogate = surrogate(key, kind, name);
        self.by.entry(surrogate.clone()).or_insert_with(|| Secret {
            name: name.into(),
            value: value.into(),
            surrogate,
            hosts: Vec::new(),
            url: false,
        })
    }

    /// The map for this vault, secrets.env (`env`, without zephyr's own names) and config.
    pub fn build(cfg: &Config, vault: &Vault, env: &[(String, String)], key: &[u8; 32]) -> Map {
        let mut map = Map::default();
        // A token bound to no host is known, and sent nowhere.
        let tokens = vault.entries().into_iter();
        for e in tokens.filter(|e| e.kind == Kind::Token && !e.raw) {
            let secret = map.insert(key, "token", &e.name, &e.value);
            secret.hosts = e.hosts.iter().filter_map(|h| parse_host(h).ok()).collect();
            secret.url = e.url;
        }
        for m in (cfg.mcp.iter()).filter(|m| m.secrets.as_deref() != Some("plain")) {
            let (names, in_url) = connector_names(m);
            let hosts = connector_hosts(m);
            for name in names {
                let Some((_, value)) = env.iter().find(|(k, _)| *k == name) else {
                    continue;
                };
                let secret = map.insert(key, "env", &name, value);
                for host in &hosts {
                    if !secret.hosts.contains(host) {
                        secret.hosts.push(host.clone());
                    }
                }
                secret.url |= in_url.contains(&name);
            }
        }
        for surrogate in map.by.keys() {
            let forms = crate::vault::base64_forms(surrogate).into_iter();
            map.forms.extend(forms.map(|f| (f, surrogate.clone())));
        }
        map
    }

    /// The map as the files are now.
    pub fn load(cfg: &Config) -> anyhow::Result<Map> {
        let vault = Vault::load(&crate::vault::path(cfg))?;
        let key = key(cfg)?;
        Ok(Map::build(cfg, &vault, &crate::drafts::secrets(cfg), &key))
    }

    /// Whether the proxy decrypts `host_port`: some secret is bound to it.
    pub fn bound(&self, host_port: &str) -> bool {
        self.by
            .values()
            .any(|s| s.hosts.iter().any(|h| h == host_port))
    }

    /// Every decrypted `host:port`, sorted.
    pub fn hosts(&self) -> Vec<String> {
        let mut all: Vec<String> = self.by.values().flat_map(|s| s.hosts.clone()).collect();
        all.sort();
        all.dedup();
        all
    }
}

/// The map the proxy swaps from, rebuilt when `vault.toml`, secrets.env, `broker.key` or the
/// connectors change: three `stat`s a look, and the files are read again only after an edit.
#[derive(Default)]
pub struct Maps {
    last: std::sync::Mutex<Option<(Stamp, std::sync::Arc<Map>)>>,
}

/// What a map was built from: each file's time and size, and the connectors.
type Stamp = (Vec<Option<(std::time::SystemTime, u64)>>, Vec<Mcp>);

impl Maps {
    /// The map as the files are now. When they cannot be read, the last one, marked
    /// `unreadable`: it still says which hosts are decrypted, so none of them falls back to a
    /// plain tunnel, and nothing is swapped from it.
    pub fn current(&self, cfg: &Config) -> std::sync::Arc<Map> {
        let files = [
            crate::vault::path(cfg),
            cfg.secrets_path(),
            cfg.state_dir().join(KEY_FILE),
        ];
        let seen = |p: &std::path::PathBuf| {
            let m = std::fs::metadata(p).ok()?;
            Some((m.modified().ok()?, m.len()))
        };
        let stamp: Stamp = (files.iter().map(seen).collect(), cfg.mcp.clone());
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, map)) = last.as_ref().filter(|(old, _)| *old == stamp) {
            return map.clone();
        }
        let map = match Map::load(cfg) {
            Ok(map) => map,
            Err(_) => Map {
                unreadable: true,
                ..last
                    .as_ref()
                    .map(|(_, m)| (**m).clone())
                    .unwrap_or_default()
            },
        };
        let map = std::sync::Arc::new(map);
        *last = Some((stamp, map.clone()));
        map
    }
}

/// Which refused tokens the owner is told about: each token and host once an hour, and ten
/// notices an hour in all, so a looping agent cannot fill a chat.
#[derive(Default)]
pub struct Notices {
    hour: i64,
    told: std::collections::HashSet<(String, String)>,
}

impl Notices {
    const AN_HOUR: usize = 10;

    pub fn due(&mut self, now: i64, token: &str, host: &str) -> bool {
        if now / 3600 != self.hour {
            self.hour = now / 3600;
            self.told.clear();
        }
        self.told.len() < Self::AN_HOUR && self.told.insert((token.into(), host.into()))
    }
}

/// Why a request that carries a surrogate is not sent on.
#[derive(Debug, PartialEq, Eq)]
pub enum Refused {
    /// A known token, on its way to a host it is not bound to.
    NotBound(String),
    /// `zsur_` that matches no token: a removed one, or made up.
    Unknown,
    /// In the path or query, for a token without `url`.
    NotInUrl(String),
    /// The real value cannot be written into a header or a URL.
    Unsendable(String),
}

impl Refused {
    /// The status line's text. It names the token and never quotes the surrogate.
    pub fn reason(&self, host: &str) -> String {
        match self {
            Refused::NotBound(name) => format!("the vault token {name} is not sent to {host}"),
            Refused::Unknown => "unknown vault surrogate".into(),
            Refused::NotInUrl(name) => format!("{name} is not swapped in URLs"),
            Refused::Unsendable(name) => format!("the vault token {name} cannot be sent as it is"),
        }
    }

    /// The token the owner is told about: one that exists and was sent where it is not bound.
    pub fn token(&self) -> Option<&str> {
        match self {
            Refused::NotBound(name) => Some(name),
            _ => None,
        }
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// `text` with each surrogate replaced by what `value` gives for its secret, in one pass;
/// `None` when it holds none. `zsur_` not followed by a known surrogate is `Unknown`.
fn replace(
    text: &[u8],
    map: &Map,
    mut value: impl FnMut(&Secret) -> Result<Vec<u8>, Refused>,
) -> Result<Option<Vec<u8>>, Refused> {
    let (mut out, mut at, mut found) = (Vec::new(), 0, false);
    while let Some(i) = text[at..]
        .windows(PREFIX.len())
        .position(|w| w == PREFIX.as_bytes())
    {
        let start = at + i;
        let end = start + PREFIX.len() + HEX;
        let secret = text
            .get(start..end)
            .and_then(|s| std::str::from_utf8(s).ok());
        let secret = secret.and_then(|s| map.by.get(s)).ok_or(Refused::Unknown)?;
        out.extend_from_slice(&text[at..start]);
        out.extend(value(secret)?);
        (at, found) = (end, true);
    }
    out.extend_from_slice(&text[at..]);
    Ok(found.then_some(out))
}

/// A surrogate's base64 form in `text`, for a token not bound to `host` (`None`: any host).
fn encoded(text: &[u8], map: &Map, host: Option<&str>) -> Result<(), Refused> {
    for (form, surrogate) in &map.forms {
        let secret = &map.by[surrogate];
        if contains(text, form.as_bytes())
            && !host.is_some_and(|h| secret.hosts.iter().any(|b| b == h))
        {
            return Err(Refused::NotBound(secret.name.clone()));
        }
    }
    Ok(())
}

/// The payload of `Basic <base64>`, decoded.
fn basic(value: &[u8]) -> Option<Vec<u8>> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    let text = std::str::from_utf8(value).ok()?;
    let (scheme, payload) = text.trim().split_once(' ')?;
    (scheme.eq_ignore_ascii_case("basic")).then(|| STANDARD.decode(payload.trim()).ok())?
}

/// Puts the real values into a request the proxy decrypted for `host` (`host:port`): in any
/// header value, in a Basic credential after decoding, and in the path and query only for a
/// token with `url`. Returns the names used. On a refusal nothing is changed. The body is not
/// this function's to see: a host that stores what it is sent would hand the token back.
pub fn swap(
    uri: &mut Uri,
    headers: &mut HeaderMap,
    map: &Map,
    host: &str,
) -> Result<Vec<String>, Refused> {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    /// The value of `s` for this host and place, noted in `used`.
    fn real(
        s: &Secret,
        host: &str,
        in_url: bool,
        used: &mut Vec<String>,
    ) -> Result<Vec<u8>, Refused> {
        if !s.hosts.iter().any(|h| h == host) {
            return Err(Refused::NotBound(s.name.clone()));
        }
        if in_url && !s.url {
            return Err(Refused::NotInUrl(s.name.clone()));
        }
        if !used.contains(&s.name) {
            used.push(s.name.clone());
        }
        Ok(s.value.clone().into_bytes())
    }
    let mut used: Vec<String> = Vec::new();
    let target = uri.path_and_query().map_or("/", |p| p.as_str()).to_string();
    encoded(target.as_bytes(), map, Some(host))?;
    let new_uri = match replace(target.as_bytes(), map, |s| real(s, host, true, &mut used))? {
        Some(bytes) => {
            let unsendable = || Refused::Unsendable(used.first().cloned().unwrap_or_default());
            let mut parts = uri.clone().into_parts();
            parts.path_and_query = Some(bytes.try_into().map_err(|_| unsendable())?);
            Some(Uri::from_parts(parts).map_err(|_| unsendable())?)
        }
        None => None,
    };
    let mut new_values = Vec::with_capacity(headers.len());
    for (name, value) in headers.iter() {
        let before = used.len();
        let decoded = (name == AUTHORIZATION)
            .then(|| basic(value.as_bytes()))
            .flatten();
        let swapped = match decoded.map(|d| replace(&d, map, |s| real(s, host, false, &mut used))) {
            Some(Ok(Some(d))) => Some(format!("Basic {}", STANDARD.encode(d)).into_bytes()),
            Some(Err(e)) => return Err(e),
            _ => replace(value.as_bytes(), map, |s| real(s, host, false, &mut used))?,
        };
        let Some(bytes) = swapped else {
            encoded(value.as_bytes(), map, Some(host))?;
            new_values.push(None);
            continue;
        };
        let mut new = HeaderValue::from_bytes(&bytes).map_err(|_| {
            Refused::Unsendable(
                used.get(before)
                    .or(used.last())
                    .cloned()
                    .unwrap_or_default(),
            )
        })?;
        new.set_sensitive(true);
        new_values.push(Some(new));
    }
    for (value, new) in headers.values_mut().zip(new_values) {
        if let Some(new) = new {
            *value = new;
        }
    }
    if let Some(new_uri) = new_uri {
        *uri = new_uri;
    }
    Ok(used)
}

/// Turns each real value of `used` in a reply's headers back into its surrogate, so a
/// `Location` or `Link` that repeats the request's URL does not hand the token over.
pub fn unswap(headers: &mut HeaderMap, map: &Map, used: &[String]) {
    let secrets: Vec<&Secret> = (map.by.values())
        .filter(|s| used.contains(&s.name) && !s.value.is_empty())
        .collect();
    for value in headers.values_mut() {
        let Ok(text) = std::str::from_utf8(value.as_bytes()) else {
            continue;
        };
        let mut new = text.to_string();
        for s in &secrets {
            new = new.replace(&s.value, &s.surrogate);
        }
        if new != text {
            if let Ok(new) = HeaderValue::from_str(&new) {
                *value = new;
            }
        }
    }
}

/// For a request that is never swapped (plain `http://`, a host name): any surrogate in
/// `texts`, or its base64 form, refuses it. A token that exists is named before an unknown one.
pub fn scan(texts: &[&[u8]], map: &Map) -> Result<(), Refused> {
    let mut unknown = false;
    let mut look = |text: &[u8]| -> Result<(), Refused> {
        let starts = (text.windows(PREFIX.len()).enumerate())
            .filter(|(_, w)| *w == PREFIX.as_bytes())
            .map(|(i, _)| i);
        for start in starts {
            let found = text.get(start..start + PREFIX.len() + HEX);
            let found = found.and_then(|s| std::str::from_utf8(s).ok());
            match found.and_then(|s| map.by.get(s)) {
                Some(secret) => return Err(Refused::NotBound(secret.name.clone())),
                None => unknown = true,
            }
        }
        encoded(text, map, None)
    };
    for text in texts {
        look(text)?;
        if let Some(decoded) = basic(text) {
            look(&decoded)?;
        }
    }
    match unknown {
        true => Err(Refused::Unknown),
        false => Ok(()),
    }
}

/// Everything `vault::scrub` hides: the vault's values, and the surrogate of each token and
/// connector secret under the same name, so the agent reads `[vault:NAME]` either way.
pub fn secrets(cfg: &Config, vault: &Vault) -> Vec<(String, String)> {
    let mut all = vault.secrets();
    // No key: no surrogate was ever handed out.
    let Ok(Some(key)) = read_key(&cfg.state_dir().join(KEY_FILE)) else {
        return all;
    };
    let tokens = vault
        .entries()
        .into_iter()
        .filter(|e| e.kind == Kind::Token);
    all.extend(tokens.map(|e| (e.name.clone(), surrogate(&key, "token", &e.name))));
    for name in cfg.mcp.iter().flat_map(|m| connector_names(m).0) {
        let pair = (name.clone(), surrogate(&key, "env", &name));
        if !all.contains(&pair) {
            all.push(pair);
        }
    }
    all
}

/// What the owner is told once after the upgrade: each token that cannot be used until it is
/// bound. `None` when there is none, or the vault cannot be read.
pub fn upgrade_notice(cfg: &Config) -> Option<String> {
    let vault = Vault::load(&crate::vault::path(cfg)).ok()?;
    let unbound = |e: &crate::vault::Entry| e.kind == Kind::Token && !e.raw && e.hosts.is_empty();
    let mut lines: Vec<String> = (vault.entries().iter().filter(|e| unbound(e)))
        .map(|e| format!("/vault bind {} HOST..", e.name))
        .collect();
    let left_out = cfg.mcp.iter().filter(|m| nowhere(m));
    lines.extend(left_out.map(|m| {
        format!(
            "/set mcp.{0}.hosts HOST.. (the connector {0} is left out until then; or /set \
             mcp.{0}.secrets plain)",
            m.name
        )
    }));
    if lines.is_empty() {
        return None;
    }
    lines.insert(
        0,
        "[zephyr] vault tokens and connector secrets now go to the agent as stand-ins, swapped for \
         the real one only on hosts you name. Until you bind one it cannot be \
         used:"
            .into(),
    );
    lines.push("To hand one out as before: /vault raw NAME on.".into());
    Some(lines.join("\n"))
}

/// How a line of `doctor` reads.
#[derive(Debug, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Fail,
}

/// `doctor`'s lines for the broker: the CA and where it is installed, that `mcp.json` holds no
/// brokered secret, which hosts are decrypted, and everything that still reaches the agent in
/// plain. `floor` is whether the container runs under the network floor; `bundle` is the
/// system's trust bundle.
pub fn doctor(
    cfg: &Config,
    floor: bool,
    now: i64,
    bundle: &std::path::Path,
) -> Vec<(Level, String)> {
    use std::os::unix::fs::PermissionsExt;
    let mut out = Vec::new();
    let mut say = |level: Level, text: String| out.push((level, text));
    if !floor {
        say(
            Level::Warn,
            "token broker off (host networking): vault tokens reach the agent in plain".into(),
        );
        return out;
    }
    let state = cfg.state_dir();
    let mode = |p: &std::path::Path| std::fs::metadata(p).map(|m| m.permissions().mode() & 0o777);
    let ca = crate::ca::dir(state);
    let private = matches!(mode(&ca), Ok(0o700)) && matches!(mode(&ca.join("ca.key")), Ok(0o600));
    match (crate::ca::days_left(state, now), private) {
        (Err(e), _) => say(Level::Fail, format!("the token broker has no CA: {e:#}")),
        (Ok(_), false) => say(
            Level::Fail,
            format!("{} must be 0700 and its ca.key 0600", ca.display()),
        ),
        (Ok(days), true) if days < 30 => say(
            Level::Warn,
            format!("the token broker's CA has {days} days left: restart with zephyr up"),
        ),
        (Ok(days), true) => say(
            Level::Ok,
            format!("the token broker's CA is the daemon's alone and has {days} days left"),
        ),
    }
    if let Ok(pem) = crate::ca::pem(state) {
        let body: String = pem.lines().filter(|l| !l.starts_with("-----")).collect();
        let holds = |p: &std::path::Path| {
            let text = std::fs::read_to_string(p).unwrap_or_default();
            let joined: String = text.lines().collect();
            !body.is_empty() && joined.contains(&body)
        };
        let public = crate::drafts::public_dir(cfg).join("ca.pem");
        for (path, what) in [(bundle, "the system bundle"), (public.as_path(), "ca.pem")] {
            let level = if holds(path) { Level::Ok } else { Level::Fail };
            let not = if level == Level::Ok { "" } else { " not" };
            say(
                level,
                format!("the CA is{not} in {what} ({})", path.display()),
            );
        }
    }
    let key = cfg.state_dir().join(KEY_FILE);
    if key.exists() && !matches!(mode(&key), Ok(0o600)) {
        say(Level::Fail, format!("{} must be 0600", key.display()));
    }
    // What the agent can read must hold no secret the proxy is there to keep from it.
    let env = crate::drafts::secrets(cfg);
    let published = std::fs::read_to_string(crate::drafts::mcp_path(cfg)).unwrap_or_default();
    let mut leaked = Vec::new();
    for m in (cfg.mcp.iter()).filter(|m| m.secrets.as_deref() != Some("plain")) {
        for name in connector_names(m).0 {
            let value = env.iter().find(|(k, _)| *k == name).map(|(_, v)| v);
            if value.is_some_and(|v| v.len() >= 4 && published.contains(v.as_str())) {
                leaked.push(format!("{name} ({})", m.name));
            }
        }
    }
    match leaked.is_empty() {
        true => say(Level::Ok, "mcp.json holds no brokered secret".into()),
        false => say(
            Level::Fail,
            format!("mcp.json holds the real secret {}", leaked.join(", ")),
        ),
    }
    let vault = match Vault::load(&crate::vault::path(cfg)) {
        Ok(vault) => vault,
        Err(e) => {
            say(Level::Fail, format!("{e:#}"));
            return out;
        }
    };
    match key_for_doctor(cfg, &vault, &env) {
        Ok(hosts) if hosts.is_empty() => {
            say(Level::Ok, "no host is decrypted: no token is bound".into())
        }
        Ok(hosts) => say(
            Level::Ok,
            format!(
                "zephyr decrypts, for every program of the agent's: {}",
                hosts.join(", ")
            ),
        ),
        Err(e) => say(Level::Fail, format!("{e:#}")),
    }
    for e in vault.entries().iter().filter(|e| e.kind == Kind::Token) {
        let name = &e.name;
        if e.raw {
            say(
                Level::Warn,
                format!("the token {name} is raw: the agent's programs get the real value"),
            );
        } else if e.hosts.is_empty() {
            say(Level::Warn, format!("the token {name} is bound to no host, so it cannot be used: /vault bind {name} HOST.."));
        } else if e.url {
            say(
                Level::Warn,
                format!("the token {name} is also swapped in URLs, which a server may repeat back"),
            );
        }
    }
    for m in &cfg.mcp {
        let name = &m.name;
        if m.secrets.as_deref() == Some("plain") && !connector_names(m).0.is_empty() {
            say(
                Level::Warn,
                format!("the connector {name} holds its real secrets (secrets = \"plain\")"),
            );
        } else if nowhere(m) {
            say(
                Level::Warn,
                format!(
                    "the connector {name} is left out: it names a secret and no host to send it to"
                ),
            );
        }
    }
    if !cfg.agent.env.is_empty() {
        let names = cfg.agent.env.join(", ");
        say(
            Level::Warn,
            format!("[agent] env hands every command of the agent the real {names}"),
        );
    }
    out
}

/// The decrypted hosts, for `doctor`; a key that cannot be read is its own failure.
fn key_for_doctor(
    cfg: &Config,
    vault: &Vault,
    env: &[(String, String)],
) -> anyhow::Result<Vec<String>> {
    let key = key(cfg).context("the token broker's key")?;
    let hosts = Map::build(cfg, vault, env, &key).hosts();
    Ok(hosts
        .iter()
        .map(|h| crate::vault::shown(h).to_string())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;
    use std::os::unix::fs::PermissionsExt;

    const KEY: [u8; 32] = [7; 32];
    const A_VALUE: &str = "ghp_realA0123456789";
    const B_VALUE: &str = "sk-realB0123456789";

    fn config(extra: &str) -> Config {
        toml::from_str(&format!("rpi = '/r'\nhome = '/nowhere'\n{extra}")).unwrap()
    }

    /// A on a.test (headers only), B on b.test:8443 (URLs too), C bound nowhere.
    fn vault() -> Vault {
        Vault::parse(&format!(
            "[[token]]\nname = \"A\"\nvalue = \"{A_VALUE}\"\nhosts = [\"a.test:443\"]\n\
             [[token]]\nname = \"B\"\nvalue = \"{B_VALUE}\"\nhosts = [\"b.test:8443\", \"a.test:443\"]\nurl = true\n\
             [[token]]\nname = \"C\"\nvalue = \"unbound-value-1\"\n\
             [[token]]\nname = \"R\"\nvalue = \"raw-value-12345\"\nraw = true\nhosts = [\"a.test:443\"]\n"
        ))
        .unwrap()
    }

    fn map() -> Map {
        Map::build(&config(""), &vault(), &[], &KEY)
    }

    fn sur(name: &str) -> String {
        surrogate(&KEY, "token", name)
    }

    fn request(target: &str, headers: &[(&str, &str)]) -> (Uri, HeaderMap) {
        let mut map = HeaderMap::new();
        for (k, v) in headers {
            let name = hyper::header::HeaderName::from_bytes(k.as_bytes()).unwrap();
            map.append(name, HeaderValue::from_str(v).unwrap());
        }
        (target.parse().unwrap(), map)
    }

    #[test]
    fn surrogates_are_stable_and_differ_by_name_kind_and_key() {
        let s = surrogate(&KEY, "token", "GH_TOKEN");
        assert_eq!(s.len(), 45);
        assert!(s.starts_with("zsur_"));
        assert!(s[5..]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)));
        assert_eq!(s, surrogate(&KEY, "token", "GH_TOKEN"));
        assert_ne!(s, surrogate(&KEY, "token", "GH_TOKEN2"));
        assert_ne!(s, surrogate(&KEY, "env", "GH_TOKEN"));
        assert_ne!(s, surrogate(&[8; 32], "token", "GH_TOKEN"));
        // The zero byte keeps `kind` and `name` apart.
        assert_ne!(
            surrogate(&KEY, "to", "ken\0X"),
            surrogate(&KEY, "token", "X")
        );
    }

    #[test]
    fn parse_host_takes_exact_names_only() {
        assert_eq!(parse_host("api.github.com").unwrap(), "api.github.com:443");
        assert_eq!(parse_host(" X.io:8443 ").unwrap(), "x.io:8443");
        for bad in [
            "*.x.io",
            "https://x.io",
            "x.io/path",
            "1.2.3.4",
            "1.2.3.4:443",
            "[::1]",
            "[::1]:443",
            "x.io:0",
            "",
            "2130706433",
            "x.io:99999",
            "user@x.io",
        ] {
            assert!(parse_host(bad).is_err(), "{bad}");
        }
        assert_eq!(
            parse_hosts("a.test, b.test:8443,"),
            ["a.test:443", "b.test:8443"]
        );
    }

    #[test]
    fn swap_puts_the_value_in_headers_and_only_there() {
        let map = map();
        let (a, b) = (sur("A"), sur("B"));
        // Bearer and a custom header.
        let (mut uri, mut h) = request(
            "/repos?x=1",
            &[("authorization", &format!("Bearer {a}")), ("x-api-key", &a)],
        );
        assert_eq!(swap(&mut uri, &mut h, &map, "a.test:443").unwrap(), ["A"]);
        assert_eq!(h["authorization"], format!("Bearer {A_VALUE}"));
        assert_eq!(h["x-api-key"], A_VALUE);
        assert!(h["x-api-key"].is_sensitive());
        assert_eq!(uri, "/repos?x=1");
        // Basic: decoded, swapped, encoded again.
        let basic = STANDARD.encode(format!("x-access-token:{a}"));
        let (mut uri, mut h) = request("/", &[("authorization", &format!("Basic {basic}"))]);
        swap(&mut uri, &mut h, &map, "a.test:443").unwrap();
        let want = STANDARD.encode(format!("x-access-token:{A_VALUE}"));
        assert_eq!(h["authorization"], format!("Basic {want}"));
        // Two tokens in one request, both bound here.
        let (mut uri, mut h) = request("/", &[("x-a", &a), ("x-b", &format!("k={b};"))]);
        assert_eq!(
            swap(&mut uri, &mut h, &map, "a.test:443").unwrap(),
            ["A", "B"]
        );
        assert_eq!(h["x-b"], format!("k={B_VALUE};"));
        // A request with none is left alone.
        let (mut uri, mut h) = request("/plain", &[("accept", "*/*")]);
        assert!(swap(&mut uri, &mut h, &map, "a.test:443")
            .unwrap()
            .is_empty());
        assert!(!h["accept"].is_sensitive());
    }

    #[test]
    fn the_target_is_swapped_only_for_a_token_with_url() {
        let map = map();
        let (mut uri, mut h) = request(&format!("/bot{}/send?key={0}", sur("B")), &[]);
        assert_eq!(swap(&mut uri, &mut h, &map, "b.test:8443").unwrap(), ["B"]);
        assert_eq!(uri, format!("/bot{B_VALUE}/send?key={B_VALUE}").as_str());
        let (mut uri, mut h) = request(&format!("/x?key={}", sur("A")), &[("x-a", &sur("A"))]);
        let refused = swap(&mut uri, &mut h, &map, "a.test:443").unwrap_err();
        assert_eq!(refused, Refused::NotInUrl("A".into()));
        assert_eq!(refused.reason("a.test:443"), "A is not swapped in URLs");
        // Refused whole: the header was not swapped either.
        assert_eq!(h["x-a"], sur("A"));
    }

    #[test]
    fn a_surrogate_is_never_swapped_on_a_host_it_is_not_bound_to() {
        let map = map();
        // A's surrogate on B's own host; an unbound token; a raw token has no surrogate at all.
        for (name, host, want) in [
            ("A", "b.test:8443", Refused::NotBound("A".into())),
            ("A", "a.test:8443", Refused::NotBound("A".into())),
            ("A", "evil.a.test:443", Refused::NotBound("A".into())),
            ("C", "a.test:443", Refused::NotBound("C".into())),
            ("R", "a.test:443", Refused::Unknown),
        ] {
            let (mut uri, mut h) =
                request("/", &[("authorization", &format!("Bearer {}", sur(name)))]);
            assert_eq!(
                swap(&mut uri, &mut h, &map, host).unwrap_err(),
                want,
                "{name} {host}"
            );
            assert_eq!(h["authorization"], format!("Bearer {}", sur(name)));
        }
        // Made up, cut short, and upper case: none is a token.
        for fake in [
            format!("zsur_{}", "0".repeat(40)),
            "zsur_abc".into(),
            sur("A").to_uppercase().replace("ZSUR", "zsur"),
        ] {
            let (mut uri, mut h) = request("/", &[("x", &fake)]);
            assert_eq!(
                swap(&mut uri, &mut h, &map, "a.test:443").unwrap_err(),
                Refused::Unknown
            );
        }
        // Base64 of a surrogate, in Basic and in any other header, to a host it is not bound to.
        let basic = STANDARD.encode(format!("u:{}", sur("A")));
        for header in [
            ("authorization", format!("Basic {basic}")),
            ("x-blob", basic.clone()),
        ] {
            let (mut uri, mut h) = request("/", &[(header.0, &header.1)]);
            let refused = swap(&mut uri, &mut h, &map, "b.test:8443").unwrap_err();
            assert_eq!(refused, Refused::NotBound("A".into()));
        }
        let (mut uri, mut h) = request(&format!("/?blob={basic}"), &[]);
        assert!(swap(&mut uri, &mut h, &map, "b.test:8443").is_err());
    }

    #[test]
    fn a_value_that_cannot_be_a_header_is_refused_by_name() {
        let vault = Vault::parse(
            "[[token]]\nname = \"NL\"\nvalue = \"line1\\nline2\"\nhosts = [\"a.test:443\"]\nurl = true\n",
        )
        .unwrap();
        let map = Map::build(&config(""), &vault, &[], &KEY);
        let (mut uri, mut h) = request("/", &[("x-key", &sur("NL"))]);
        let refused = swap(&mut uri, &mut h, &map, "a.test:443").unwrap_err();
        assert_eq!(refused, Refused::Unsendable("NL".into()));
        assert_eq!(h["x-key"], sur("NL"));
        let (mut uri, mut h) = request(&format!("/?k={}", sur("NL")), &[]);
        let refused = swap(&mut uri, &mut h, &map, "a.test:443").unwrap_err();
        assert_eq!(refused, Refused::Unsendable("NL".into()));
        assert!(!refused.reason("a.test:443").contains("line1"));
    }

    #[test]
    fn unswap_turns_a_reply_header_back() {
        let map = map();
        let (_, mut h) = request(
            "/",
            &[
                ("location", &format!("https://b.test/bot{B_VALUE}/x")),
                ("x-other", A_VALUE),
            ],
        );
        unswap(&mut h, &map, &["B".into()]);
        assert_eq!(h["location"], format!("https://b.test/bot{}/x", sur("B")));
        // Only what this request carried.
        assert_eq!(h["x-other"], A_VALUE);
    }

    #[test]
    fn scan_refuses_any_surrogate() {
        let map = map();
        let a = sur("A");
        let basic = format!("Basic {}", STANDARD.encode(format!("u:{a}")));
        let b64 = STANDARD.encode(format!("xx{a}"));
        for text in [
            format!("Bearer {a}"),
            format!("/path?key={a}"),
            format!("{a}.evil.example:443"),
            basic,
            b64,
        ] {
            let refused = scan(&[b"harmless", text.as_bytes()], &map).unwrap_err();
            assert_eq!(refused, Refused::NotBound("A".into()), "{text}");
        }
        let fake = format!("zsur_{}", "f".repeat(40));
        assert_eq!(scan(&[fake.as_bytes()], &map), Err(Refused::Unknown));
        // A known one is named even beside an unknown one.
        let both = format!("{fake} {a}");
        assert_eq!(
            scan(&[both.as_bytes()], &map),
            Err(Refused::NotBound("A".into()))
        );
        assert_eq!(scan(&[b"GET / HTTP/1.1", b"zsu_r"], &map), Ok(()));
    }

    #[test]
    fn nothing_printed_holds_a_value_or_a_surrogate() {
        let map = map();
        let mut printed = format!("{map:?}");
        for refused in [
            Refused::NotBound("A".into()),
            Refused::Unknown,
            Refused::NotInUrl("A".into()),
            Refused::Unsendable("A".into()),
        ] {
            printed.push_str(&refused.reason("evil.example:443"));
            printed.push_str(&format!("{refused:?}"));
        }
        assert!(
            printed.contains("a.test:443") && printed.contains("\"A\""),
            "{printed}"
        );
        for hidden in [A_VALUE, B_VALUE, "unbound-value-1", "zsur_"] {
            assert!(!printed.contains(hidden), "{hidden} in {printed}");
        }
        assert_eq!(
            Refused::NotBound("GH_TOKEN".into()).reason("evil.example:443"),
            "the vault token GH_TOKEN is not sent to evil.example:443"
        );
    }

    #[test]
    fn the_key_is_made_once_and_private() {
        let dir = crate::test_dir("broker-key");
        let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = {dir:?}\n")).unwrap();
        let vault = vault();
        // Before any key there is no surrogate to scrub, and scrubbing makes none.
        assert_eq!(secrets(&cfg, &vault), vault.secrets());
        assert!(!dir.join(KEY_FILE).exists());
        let key = key(&cfg).unwrap();
        let mode = std::fs::metadata(dir.join(KEY_FILE))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        assert_eq!(super::key(&cfg).unwrap(), key);
        std::fs::write(dir.join(KEY_FILE), "short").unwrap();
        let e = super::key(&cfg).unwrap_err();
        assert!(!format!("{e:#}").contains("short"), "{e:#}");
    }

    #[test]
    fn secrets_scrub_a_surrogate_and_its_base64_form() {
        let dir = crate::test_dir("broker-scrub");
        let cfg: Config = toml::from_str(&format!(
            "rpi = '/r'\nhome = {dir:?}\n[[mcp]]\nname = \"notes\"\nurl = \"https://n.test/mcp\"\n\
             headers = {{ Authorization = \"Bearer ${{NOTES_KEY}}\" }}\n"
        ))
        .unwrap();
        let key = key(&cfg).unwrap();
        let all = secrets(&cfg, &vault());
        let a = surrogate(&key, "token", "A");
        let text = format!(
            "A={a} basic={} env={} value={A_VALUE}",
            STANDARD.encode(format!("u:{a}")),
            surrogate(&key, "env", "NOTES_KEY"),
        );
        let clean = crate::vault::scrub(&text, &all);
        assert!(
            !clean.contains("zsur_") && !clean.contains(A_VALUE),
            "{clean}"
        );
        assert!(clean.starts_with("A=[vault:A] basic="), "{clean}");
        assert!(
            clean.contains("env=[vault:NOTES_KEY] value=[vault:A]"),
            "{clean}"
        );
    }

    #[test]
    fn the_map_binds_connector_secrets_to_their_hosts() {
        let cfg = config(
            "[[mcp]]\nname = \"notes\"\nurl = \"https://n.test/mcp?key=${URL_KEY}\"\n\
             headers = { Authorization = \"Bearer ${SHARED}\" }\n\
             [[mcp]]\nname = \"local\"\ncommand = \"srv\"\nenv = { K = \"${SHARED}\", P = \"${PLAINLY}\" }\n\
             hosts = \"api.l.test, l.test:8443\"\n\
             [[mcp]]\nname = \"db\"\ncommand = \"db\"\nenv = { P = \"${PLAINLY}\" }\nsecrets = \"plain\"\n\
             [[mcp]]\nname = \"lan\"\nurl = \"http://lan.test/mcp\"\nheaders = { K = \"${LAN}\" }\n",
        );
        let env: Vec<(String, String)> = ["URL_KEY", "SHARED", "PLAINLY", "LAN"]
            .iter()
            .map(|k| (k.to_string(), format!("value-of-{k}")))
            .collect();
        let map = Map::build(&cfg, &Vault::parse("").unwrap(), &env, &KEY);
        let of = |name: &str| &map.by[&surrogate(&KEY, "env", name)];
        assert_eq!(of("URL_KEY").hosts, ["n.test:443"]);
        assert!(of("URL_KEY").url && !of("SHARED").url);
        assert_eq!(
            of("SHARED").hosts,
            ["n.test:443", "api.l.test:443", "l.test:8443"]
        );
        // Only the surrogate-mode connector's use of it counts.
        assert_eq!(of("PLAINLY").hosts, ["api.l.test:443", "l.test:8443"]);
        // Plain http is never swapped: known, bound nowhere.
        assert!(of("LAN").hosts.is_empty());
        assert_eq!(map.hosts(), ["api.l.test:443", "l.test:8443", "n.test:443"]);
        assert!(map.bound("n.test:443") && !map.bound("n.test:80") && !map.bound("lan.test:80"));
        assert_eq!(
            connector_names(&cfg.mcp[0]),
            (
                vec!["URL_KEY".to_string(), "SHARED".into()],
                vec!["URL_KEY".to_string()]
            )
        );
    }
    #[test]
    fn doctor_lists_decrypted_hosts_and_warns_about_plain_secrets() {
        let dir = crate::test_dir("broker-doctor");
        let state = dir.join("state");
        std::fs::create_dir_all(&state).unwrap();
        let cfg: Config = toml::from_str(&format!(
            "rpi = '/r'\nhome = {dir:?}\nstate = {state:?}\n[agent]\nenv = ['OPENAI_API_KEY']\n\
             [[mcp]]\nname = 'docs'\nurl = 'https://docs.test/mcp'\nheaders = {{ A = '${{DOCS_TOKEN}}' }}\n\
             [[mcp]]\nname = 'db'\ncommand = 'db'\nenv = {{ P = '${{DB_PASSWORD}}' }}\nsecrets = 'plain'\n\
             [[mcp]]\nname = 'local'\ncommand = 'srv'\nenv = {{ K = '${{DOCS_TOKEN}}' }}\n"
        ))
        .unwrap();
        let bundle = dir.join("bundle.crt");
        let text = |lines: &[(Level, String)]| -> String {
            let line = |(l, t): &(Level, String)| format!("{l:?} {t}\n");
            lines.iter().map(line).collect()
        };
        // Under host networking: the one line, and nothing else is looked at.
        assert_eq!(
            doctor(&cfg, false, 0, &bundle),
            [(
                Level::Warn,
                "token broker off (host networking): vault tokens reach the agent in plain".into()
            )]
        );
        // No CA yet: a failure, by name.
        let now = crate::now();
        assert!(
            text(&doctor(&cfg, true, now, &bundle)).starts_with("Fail the token broker has no CA")
        );
        crate::ca::ensure(&state, now).unwrap();
        let pem = crate::ca::pem(&state).unwrap();
        std::fs::write(
            &bundle,
            format!("-----BEGIN CERTIFICATE-----\nother\n-----END CERTIFICATE-----\n{pem}"),
        )
        .unwrap();
        let public = crate::drafts::ensure_public_dir(&cfg).unwrap();
        std::fs::write(public.join("ca.pem"), &pem).unwrap();
        std::fs::write(
            cfg.secrets_path(),
            "DOCS_TOKEN=real-docs-token\nDB_PASSWORD=real-db-password\n",
        )
        .unwrap();
        std::fs::write(
            crate::vault::path(&cfg),
            "[[token]]\nname = \"BOUND\"\nvalue = \"v-bound-1\"\nhosts = [\"api.x.test:443\", \"x.test:8443\"]\n\
             [[token]]\nname = \"RAW\"\nvalue = \"v-raw-12\"\nraw = true\n\
             [[token]]\nname = \"URL\"\nvalue = \"v-url-12\"\nhosts = [\"u.test:443\"]\nurl = true\n\
             [[token]]\nname = \"LOOSE\"\nvalue = \"v-loose-1\"\n",
        )
        .unwrap();
        std::fs::write(
            crate::drafts::mcp_path(&cfg),
            "{\"mcpServers\": {\"db\": \"real-db-password\"}}",
        )
        .unwrap();
        let said = text(&doctor(&cfg, true, now, &bundle));
        for line in [
            "Ok the token broker's CA is the daemon's alone and has 397 days left\n",
            "Ok the CA is in the system bundle (",
            "Ok the CA is in ca.pem (",
            "Ok mcp.json holds no brokered secret\n",
            "Ok zephyr decrypts, for every program of the agent's: api.x.test, docs.test, u.test, x.test:8443\n",
            "Warn the token RAW is raw: the agent's programs get the real value\n",
            "Warn the token URL is also swapped in URLs",
            "Warn the token LOOSE is bound to no host, so it cannot be used: /vault bind LOOSE HOST..\n",
            "Warn the connector db holds its real secrets (secrets = \"plain\")\n",
            "Warn the connector local is left out: it names a secret and no host to send it to\n",
            "Warn [agent] env hands every command of the agent the real OPENAI_API_KEY\n",
        ] {
            assert!(said.contains(line), "{line} not in:\n{said}");
        }
        assert!(!said.contains("Fail"), "{said}");
        for hidden in [
            "v-bound-1",
            "v-raw-12",
            "real-docs-token",
            "real-db-password",
            "zsur_",
        ] {
            assert!(!said.contains(hidden), "{hidden} in:\n{said}");
        }
        // The published file holding a brokered secret is a failure; so is a CA that is not
        // installed, or a key folder others may enter.
        std::fs::write(crate::drafts::mcp_path(&cfg), "Bearer real-docs-token").unwrap();
        std::fs::write(&bundle, "").unwrap();
        std::fs::set_permissions(
            crate::ca::dir(&state),
            std::fs::Permissions::from_mode(0o755),
        )
        .unwrap();
        let said = text(&doctor(&cfg, true, now, &bundle));
        for line in [
            "Fail mcp.json holds the real secret DOCS_TOKEN (docs)",
            "Fail the CA is not in the system bundle (",
            "must be 0700 and its ca.key 0600\n",
        ] {
            assert!(said.contains(line), "{line} not in:\n{said}");
        }
        assert!(!said.contains("real-docs-token"), "{said}");
    }
}
