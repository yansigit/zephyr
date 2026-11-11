//! The connector catalog (DESIGN §4.22): ready-made `[[mcp]]` tables that ship in the image as
//! `/opt/zephyr/connectors/NAME.toml`, root's. Each says its command or URL, the secrets it
//! needs, the hosts they are bound to (§4.21) and the gate classes it presets (§4.16). Pure: the
//! caller reads the file (the daemon from `Config::catalog_dir`, the host CLI through the
//! container), so the image's copy is the only one.

use crate::config::Mcp;
use anyhow::{bail, Context};
use serde::Deserialize;
use std::path::Path;

/// Where the image keeps the catalog.
pub const DIR: &str = "/opt/zephyr/connectors";

/// One secret an entry needs, with where the owner gets it.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Secret {
    pub name: String,
    pub help: String,
}

/// A catalog entry as the file says it. The image and its binary are one release, so an
/// unknown key is a bug, refused.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
struct File {
    about: String,
    #[serde(default)]
    plain_reason: Option<String>,
    #[serde(default)]
    needs_url: bool,
    #[serde(default, rename = "secret")]
    secrets: Vec<Secret>,
    server: toml::Table,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub name: String,
    pub about: String,
    /// Why the owner must say `plain` for this one; `None` when stand-ins work.
    pub plain_reason: Option<String>,
    /// The owner gives a URL, which fills `{url}`; `{host}` is its `host[:port]`.
    pub needs_url: bool,
    pub secrets: Vec<Secret>,
    server: Mcp,
}

/// Parses the entry `name` (its file's name, without `.toml`).
pub fn parse(name: &str, text: &str) -> anyhow::Result<Entry> {
    if !crate::drafts::valid_name(name) {
        bail!("catalog entry {name:?}: a name is 1-64 lowercase letters, digits or '-'");
    }
    let file: File = toml::from_str(text).with_context(|| format!("catalog entry {name}"))?;
    let mut server = file.server;
    // Whether a connector holds its real secrets is only ever the owner's word.
    for key in ["name", "secrets"] {
        if server.contains_key(key) {
            bail!("catalog entry {name}: [server] cannot set {key}");
        }
    }
    server.insert("name".into(), name.into());
    let server: Mcp = server
        .try_into()
        .with_context(|| format!("catalog entry {name}: [server] is not an [[mcp]] table"))?;
    let placeholders = |m: &Mcp| {
        let mut texts = m.url.iter().chain(&m.args).chain(std::iter::once(&m.hosts));
        texts.any(|t| t.contains("{url}") || t.contains("{host}"))
    };
    if placeholders(&server) != file.needs_url {
        bail!("catalog entry {name}: {{url}} and {{host}} appear exactly when needs_url is true");
    }
    Ok(Entry {
        name: name.into(),
        about: file.about,
        plain_reason: file.plain_reason,
        needs_url: file.needs_url,
        secrets: file.secrets,
        server,
    })
}

impl Entry {
    /// The `[[mcp]]` table this entry writes, `{url}` and `{host}` filled from `url`.
    pub fn to_mcp(&self, url: Option<&str>) -> anyhow::Result<Mcp> {
        let mut m = self.server.clone();
        match (self.needs_url, url) {
            (true, None) => bail!("{} needs the server's URL", self.name),
            (false, Some(_)) => bail!("{} takes no URL", self.name),
            (false, None) => return Ok(m),
            (true, Some(url)) => {
                let parsed = url::Url::parse(url).with_context(|| format!("{url}: not a URL"))?;
                if !matches!(parsed.scheme(), "https" | "http") {
                    bail!("{url}: give an https:// or http:// URL");
                }
                let host = parsed.host_str().context("the URL has no host")?;
                let host = match parsed.port() {
                    Some(port) => format!("{host}:{port}"),
                    None => host.to_string(),
                };
                let fill = |t: &str| t.replace("{url}", url).replace("{host}", &host);
                m.url = m.url.as_deref().map(fill);
                m.args = m.args.iter().map(|a| fill(a)).collect();
                m.hosts = fill(&m.hosts);
            }
        }
        Ok(m)
    }

    /// Whether `m`, the owner's connector of this name, runs what this entry ships: its
    /// command, arguments and URL, where an argument holding `{url}` is the owner's.
    pub fn same_as(&self, m: &Mcp) -> bool {
        let arg = |(ours, theirs): (&String, &String)| ours.contains("{url}") || ours == theirs;
        m.command == self.server.command
            && m.url == self.server.url
            && m.args.len() == self.server.args.len()
            && self.server.args.iter().zip(&m.args).all(arg)
    }
}

impl Entry {
    /// The table as the file has it, `{url}` and `{host}` unfilled.
    pub fn template(&self) -> &Mcp {
        &self.server
    }

    /// Why this entry, at `url`, can only hold its real key, which the owner must accept with
    /// `plain`: its own `plain_reason`, or an `http://` URL, where the proxy swaps nothing.
    pub fn plain_needed(&self, url: Option<&str>) -> Option<String> {
        if let Some(why) = &self.plain_reason {
            return Some(why.clone());
        }
        let http = url.is_some_and(|u| u.starts_with("http://"));
        (http && !self.secrets.is_empty()).then(|| {
            format!(
                "{} at an http:// URL gets its key as it is: zephyr swaps a stand-in only over HTTPS",
                self.name
            )
        })
    }
}

/// The connector to write for `entry`'s table `new`, over the owner's `old` one of that name (D4):
/// the owner's hosts stay unless `hosts` names some, the owner's class wins per tool, and
/// `secrets = "plain"` is written only when `plain` says so (an old `plain` stays).
pub fn merged(mut new: Mcp, old: Option<&Mcp>, hosts: &[String], plain: bool) -> Mcp {
    if let Some(old) = old {
        if !old.hosts.trim().is_empty() {
            new.hosts = old.hosts.clone();
        }
        new.classes.extend(old.classes.clone());
        new.secrets = old.secrets.clone();
    }
    if !hosts.is_empty() {
        new.hosts = hosts.join(", ");
    }
    if plain {
        new.secrets = Some("plain".into());
    }
    new
}

/// The entry `dir/NAME.toml`.
pub fn read(dir: &Path, name: &str) -> anyhow::Result<Entry> {
    if !crate::drafts::valid_name(name) {
        bail!("no catalog entry named {name:?}");
    }
    let path = dir.join(format!("{name}.toml"));
    let text = std::fs::read_to_string(&path).with_context(|| {
        format!("no catalog entry named {name} (zephyr mcp catalog lists them)")
    })?;
    parse(name, &text)
}

/// Every entry in `dir`, by name. One that does not parse is logged and left out.
pub fn list(dir: &Path) -> Vec<Entry> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter_map(|n| n.strip_suffix(".toml").map(String::from))
        .collect();
    names.sort();
    let read = |n: &String| read(dir, n).map_err(|e| eprintln!("zephyr: {e:#}")).ok();
    names.iter().filter_map(read).collect()
}

/// `doctor`'s notes: each connector of a catalog entry's name that no longer runs what the
/// entry ships.
pub fn newer(cfg: &crate::config::Config) -> Vec<String> {
    let entries = list(&cfg.catalog_dir);
    let stale = |m: &&Mcp| entries.iter().any(|e| e.name == m.name && !e.same_as(m));
    (cfg.mcp.iter().filter(stale))
        .map(|m| {
            format!(
                "a newer catalog entry for {0} ships: zephyr mcp add --catalog {0}",
                m.name
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use std::path::PathBuf;

    fn shipped() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../deploy/connectors")
    }

    fn config_with(m: Mcp) -> Config {
        let mut cfg: Config = toml::from_str("rpi = \"/x\"\nhome = \"/h\"\n").unwrap();
        cfg.mcp.push(m);
        cfg
    }

    #[test]
    fn every_shipped_entry_parses_and_validates() {
        let names: Vec<String> = std::fs::read_dir(shipped())
            .unwrap()
            .map(|e| e.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 6, "{names:?}");
        for file in names {
            let name = file.strip_suffix(".toml").expect("only .toml files");
            let text = std::fs::read_to_string(shipped().join(&file)).unwrap();
            let e = parse(name, &text).unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert!(crate::drafts::valid_name(name));
            let url = e.needs_url.then_some("https://ha.example.com:8123/api/mcp");
            let m = e.to_mcp(url).unwrap();
            m.check().unwrap_or_else(|e| panic!("{name}: {e:#}"));
            assert!(m.secrets.is_none(), "{name} sets secrets");
            let cfg = config_with(m.clone());
            cfg.validate().unwrap_or_else(|e| panic!("{name}: {e:#}"));
            let (used, _) = crate::broker::connector_names(&m);
            let declared: Vec<&str> = e.secrets.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(
                used, declared,
                "{name}: the secrets it uses are the ones it lists"
            );
            for s in &declared {
                assert!(!s.starts_with("ZEPHYR_"), "{name}: {s}");
            }
            if m.command.is_some() && !used.is_empty() {
                assert!(
                    !crate::broker::connector_hosts(&m).is_empty() || e.plain_reason.is_some(),
                    "{name}: a command connector with a secret needs hosts or a plain_reason"
                );
            }
            // Pinned: no `npx` without an exact version.
            if m.command.as_deref() == Some("npx") {
                let pkg = m.args.iter().find(|a| !a.starts_with('-')).unwrap();
                let version = pkg.rsplit_once('@').map(|v| v.1).unwrap_or_default();
                assert!(
                    version.split('.').count() == 3
                        && version.chars().all(|c| c.is_ascii_digit() || c == '.'),
                    "{name}: {pkg} is not pinned"
                );
            }
        }
    }

    #[test]
    fn presets_change_only_what_the_verb_rule_gets_wrong() {
        let github = read(&shipped(), "github").unwrap().to_mcp(None).unwrap();
        // Every preset is a post, the gh gate's list whole; some are posts by their names too.
        let mut by_name = Vec::new();
        for (tool, class) in &github.classes {
            assert_eq!(class, "posts");
            if crate::approvals::verb_class("github", tool) == "posts" {
                by_name.push(tool.as_str());
            }
        }
        assert_eq!(
            by_name,
            ["add_comment_to_pending_review", "update_issue_comment"]
        );
        for tool in ["add_issue_comment", "create_pull_request"] {
            assert_eq!(
                crate::approvals::verb_class("github", tool),
                "posts",
                "{tool}"
            );
        }
        let notion = read(&shipped(), "notion").unwrap().to_mcp(None).unwrap();
        assert_eq!(notion.classes["API-post-search"], "none");
        assert_eq!(
            crate::approvals::verb_class("notion", "API-post-search"),
            "posts"
        );
    }

    #[test]
    fn an_unknown_key_and_a_secrets_key_are_refused() {
        let ok = "about = \"x\"\n[server]\nurl = \"https://x.test/mcp\"\n";
        parse("x", ok).unwrap();
        let unknown = format!("{ok}colour = \"red\"\n");
        assert!(parse("x", &unknown).is_err());
        let top = format!("catalog = 1\n{ok}");
        assert!(parse("x", &top).is_err());
        let plain = format!("{ok}secrets = \"plain\"\n");
        let e = parse("x", &plain).unwrap_err();
        assert!(format!("{e:#}").contains("cannot set secrets"), "{e:#}");
        assert!(parse("X", ok).is_err(), "a name the adapter refuses");
    }

    #[test]
    fn to_mcp_fills_url_and_host_and_refuses_the_wrong_shape() {
        let ha = read(&shipped(), "home-assistant").unwrap();
        let m = ha
            .to_mcp(Some("https://ha.example.com:8123/api/mcp"))
            .unwrap();
        assert_eq!(m.hosts, "ha.example.com:8123");
        assert!(m
            .args
            .contains(&"https://ha.example.com:8123/api/mcp".to_string()));
        assert!(!m.args.iter().any(|a| a.contains("{url}")));
        assert!(ha.to_mcp(None).is_err());
        assert!(ha.to_mcp(Some("ftp://ha.example.com/")).is_err());
        let github = read(&shipped(), "github").unwrap();
        assert!(github.to_mcp(Some("https://x.test/")).is_err());
        // A {url} entry with no placeholder, or a placeholder without needs_url, is a bug.
        let no = "about = \"x\"\nneeds_url = true\n[server]\ncommand = \"c\"\n";
        assert!(parse("x", no).is_err());
        let stray = "about = \"x\"\n[server]\ncommand = \"c\"\nargs = [\"{url}\"]\n";
        assert!(parse("x", stray).is_err());
    }

    #[test]
    fn same_as_tells_a_newer_entry() {
        let github = read(&shipped(), "github").unwrap();
        let mut m = github.to_mcp(None).unwrap();
        m.hosts = "api.github.com".into();
        m.classes.clear();
        assert!(
            github.same_as(&m),
            "the owner's hosts and classes are theirs"
        );
        m.args = vec!["stdio".into(), "--read-only".into()];
        assert!(!github.same_as(&m));
        let mut cfg = config_with(m);
        cfg.catalog_dir = shipped();
        assert_eq!(
            newer(&cfg),
            ["a newer catalog entry for github ships: zephyr mcp add --catalog github"]
        );
        cfg.mcp[0] = github.to_mcp(None).unwrap();
        assert!(newer(&cfg).is_empty());
        let ha = read(&shipped(), "home-assistant").unwrap();
        let mine = ha.to_mcp(Some("https://home.test/api/mcp")).unwrap();
        assert!(ha.same_as(&mine), "the owner's URL is no newer entry");
    }
}
