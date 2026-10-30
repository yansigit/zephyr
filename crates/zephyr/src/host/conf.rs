//! The files under `~/.zephyr`: `zephyr.toml` edited with toml_edit (comments survive),
//! `secrets.env` and `tui.token` (mode 0600), and `host.toml` (host-only settings).

use anyhow::{bail, Context};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// Provider extensions baked into the image (deploy/Dockerfile).
pub const EXTENSIONS: [&str; 3] = [
    "/opt/zephyr/ext/librpi_providers.so",
    "/opt/zephyr/ext/rpi-cursor-lite/librpi_cursor_lite.so",
    "/opt/zephyr/ext/rpi-claude-agent/librpi_claude_agent.so",
];

pub struct Data {
    pub dir: PathBuf,
}

impl Data {
    pub fn config(&self) -> PathBuf {
        self.dir.join("zephyr.toml")
    }
    pub fn secrets(&self) -> PathBuf {
        self.dir.join("secrets.env")
    }
    pub fn token(&self) -> PathBuf {
        self.dir.join("tui.token")
    }
    pub fn host(&self) -> PathBuf {
        self.dir.join("host.toml")
    }
    pub fn account(&self, name: &str) -> PathBuf {
        self.dir.join("accounts").join(name)
    }

    /// Creates what is missing: the dir (0700), `zephyr.toml`, `tui.token`, `secrets.env`, the agent dir.
    pub fn ensure_layout(&self) -> anyhow::Result<()> {
        for dir in [
            &self.dir,
            &self.dir.join(".rpi/agent"),
            &self.dir.join("accounts"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        if !self.config().exists() {
            std::fs::write(self.config(), default_config(unsafe { libc::getuid() }))?;
        }
        if !self.token().exists() {
            write_private(&self.token(), &random_hex(24)?)?;
        }
        if !self.secrets().exists() {
            write_private(
                &self.secrets(),
                "# KEY=value lines; zephyr picks up a change by itself\n",
            )?;
        }
        Ok(())
    }

    pub fn load(&self) -> anyhow::Result<DocumentMut> {
        load(&self.config())
    }

    pub fn save(&self, doc: &DocumentMut) -> anyhow::Result<()> {
        save(&self.config(), doc)
    }

    /// `projects` from `host.toml`.
    pub fn projects(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(self.host_get("projects")?.map(PathBuf::from))
    }

    /// A key of `host.toml`, e.g. `memory` (the container's `--memory` limit).
    pub fn host_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        if !self.host().exists() {
            return Ok(None);
        }
        Ok(get(&load(&self.host())?, key))
    }

    pub fn set_projects(&self, path: &Path) -> anyhow::Result<()> {
        let mut doc = if self.host().exists() {
            load(&self.host())?
        } else {
            DocumentMut::new()
        };
        doc["projects"] = value(path.display().to_string());
        Ok(std::fs::write(self.host(), doc.to_string())?)
    }
}

/// Whole or not at all: the running daemon reloads the file when its mtime changes.
pub fn save(path: &Path, doc: &DocumentMut) -> anyhow::Result<()> {
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, doc.to_string())
        .and_then(|()| std::fs::rename(&tmp, path))
        .with_context(|| format!("writing {}", path.display()))
}

pub fn load(path: &Path) -> anyhow::Result<DocumentMut> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} (run `zephyr init` first)", path.display()))?;
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

fn default_config(uid: u32) -> String {
    let ext = EXTENSIONS.map(|e| format!("\"{e}\"")).join(", ");
    format!(
        r#"# Zephyr's config, seen as /zephyr/zephyr.toml in the container. Edit with `zephyr config set`.
rpi = "/usr/local/bin/rpi"
home = "/zephyr"
agent_dir = "/zephyr/.rpi/agent"
policy_plugin = "/usr/local/lib/libzephyr_policy.so"
extensions = [{ext}]
# model = "provider/model-id"   # `zephyr model` picks one

[owners]
chat = ["{uid}"]

[attach]
port = 7790
token_file = "/zephyr/tui.token"
"#
    )
}

/// Writes `text` to `path` with mode 0600, whatever the file's mode was.
pub fn write_private(path: &Path, text: &str) -> anyhow::Result<()> {
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
    file.write_all(text.as_bytes())?;
    Ok(())
}

pub fn random_hex(bytes: usize) -> anyhow::Result<String> {
    let mut buf = vec![0u8; bytes];
    std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut buf)?;
    Ok(buf.iter().map(|b| format!("{b:02x}")).collect())
}

/// Sets `key` in a dotenv file, replacing its line or appending one. The file stays 0600.
pub fn set_secret(path: &Path, key: &str, secret: &str) -> anyhow::Result<()> {
    if secret.contains(['\n', '\r']) {
        bail!("{key}: the value must be one line");
    }
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let line = format!("{key}={secret}");
    let mut lines: Vec<&str> = text.lines().collect();
    match lines
        .iter()
        .position(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == key))
    {
        Some(i) => lines[i] = &line,
        None => lines.push(&line),
    }
    write_private(path, &(lines.join("\n") + "\n"))
}

/// A dotted key, e.g. `attach.port`, as a display string (strings without quotes, string lists comma separated).
pub fn get(doc: &DocumentMut, key: &str) -> Option<String> {
    let mut item = doc.as_item();
    for part in key.split('.') {
        item = item.get(part)?;
    }
    let strings: Option<Vec<&str>> = item
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_str()).collect());
    Some(match (item.as_str(), strings) {
        (Some(s), _) => s.to_string(),
        (None, Some(list)) if !list.is_empty() => list.join(", "),
        _ => match item.as_value() {
            Some(v) => {
                let mut v = v.clone();
                v.decor_mut().clear();
                v.to_string()
            }
            None => item.to_string(),
        }
        .trim()
        .to_string(),
    })
}

/// Sets a dotted key, creating tables on the way. `raw` is read as a TOML value, else as a string.
/// A value already there keeps its trailing comment.
pub fn set(doc: &mut DocumentMut, key: &str, raw: &str) -> anyhow::Result<()> {
    let mut new: toml_edit::Value = raw.parse().unwrap_or_else(|_| raw.into());
    let parts: Vec<&str> = key.split('.').collect();
    let (last, path) = parts.split_last().context("empty key")?;
    let mut table = doc.as_table_mut();
    for part in path {
        let item = table
            .entry(part)
            .or_insert_with(|| Item::Table(Table::new()));
        table = item
            .as_table_mut()
            .with_context(|| format!("{part} in {key} is not a table"))?;
    }
    if let Some(old) = table.get(last).and_then(Item::as_value) {
        *new.decor_mut() = old.decor().clone();
    }
    table[last] = Item::Value(new);
    Ok(())
}

/// `[[account]]` entries as (name, model), in failover order.
pub fn accounts(doc: &DocumentMut) -> Vec<(String, String)> {
    doc.get("account")
        .and_then(Item::as_array_of_tables)
        .map(|a| {
            a.iter()
                .map(|t| {
                    let s = |k| {
                        t.get(k)
                            .and_then(Item::as_str)
                            .unwrap_or_default()
                            .to_string()
                    };
                    (s("name"), s("model"))
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Adds the account, or sets the model of the one with that name.
pub fn upsert_account(doc: &mut DocumentMut, name: &str, model: &str) {
    let list = doc
        .entry("account")
        .or_insert_with(|| Item::ArrayOfTables(ArrayOfTables::new()))
        .as_array_of_tables_mut()
        .expect("account is an array of tables");
    let found = list
        .iter()
        .position(|t| t.get("name").and_then(Item::as_str) == Some(name));
    match found {
        Some(i) => list.get_mut(i).expect("found")["model"] = value(model),
        None => {
            let mut t = Table::new();
            t["name"] = value(name);
            t["model"] = value(model);
            list.push(t);
        }
    }
}

/// Removes the named account; false when there is none.
pub fn remove_account(doc: &mut DocumentMut, name: &str) -> bool {
    let Some(list) = doc
        .get_mut("account")
        .and_then(Item::as_array_of_tables_mut)
    else {
        return false;
    };
    let before = list.len();
    list.retain(|t| t.get("name").and_then(Item::as_str) != Some(name));
    before != list.len()
}

/// Puts the accounts in the order given, which must name each account exactly once.
pub fn order_accounts(doc: &mut DocumentMut, names: &[String]) -> anyhow::Result<()> {
    let mut have: Vec<String> = accounts(doc).into_iter().map(|(n, _)| n).collect();
    let mut want = names.to_vec();
    have.sort();
    want.sort();
    if have != want {
        bail!("name every account once: {}", have.join(" "));
    }
    let list = doc["account"]
        .as_array_of_tables_mut()
        .expect("checked above");
    let mut tables: Vec<Table> = list.iter().cloned().collect();
    tables.sort_by_key(|t| {
        let n = t.get("name").and_then(Item::as_str).unwrap_or_default();
        names.iter().position(|x| x == n)
    });
    // The document prints tables by position, so the new order takes over the old slots.
    let slots: Vec<Option<isize>> = list.iter().map(Table::position).collect();
    list.clear();
    for (mut t, slot) in tables.into_iter().zip(slots) {
        t.set_position(slot);
        list.push(t);
    }
    Ok(())
}

/// Adds the image's extensions a config lacks (one written before they were baked in).
pub fn add_image_extensions(doc: &mut DocumentMut) {
    let list = doc
        .entry("extensions")
        .or_insert_with(|| value(toml_edit::Array::new()));
    if let Some(list) = list.as_array_mut() {
        for e in EXTENSIONS {
            if !list.iter().any(|v| v.as_str() == Some(e)) {
                list.push(e);
            }
        }
    }
}

/// `--extension <path>` for each configured extension, for rpi calls in the container.
pub fn extension_args(doc: &DocumentMut) -> Vec<String> {
    doc.get("extensions")
        .and_then(Item::as_array)
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str())
        .flat_map(|e| ["--extension".to_string(), e.to_string()])
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = r#"# top comment
rpi = "/usr/local/bin/rpi"   # pinned
model = "a/b"   # the default model

[attach]
port = 7790   # loopback only

# the pool
[[account]]
name = "x"   # first
model = "p/m"
"#;

    #[test]
    fn edits_keep_comments() {
        let mut doc: DocumentMut = ORIGINAL.parse().unwrap();
        set(&mut doc, "model", "openai-codex/gpt-5.5").unwrap();
        set(&mut doc, "attach.port", "7791").unwrap();
        set(&mut doc, "budget.daily_tokens", "1000").unwrap();
        upsert_account(&mut doc, "y", "q/n");
        upsert_account(&mut doc, "x", "p/m2");
        let text = doc.to_string();
        for comment in [
            "# top comment",
            "# pinned",
            "# the default model",
            "# loopback only",
            "# the pool",
            "# first",
        ] {
            assert!(text.contains(comment), "lost {comment:?}:\n{text}");
        }
        assert!(
            text.contains("model = \"openai-codex/gpt-5.5\"   # the default model"),
            "{text}"
        );
        assert!(text.contains("port = 7791   # loopback only"), "{text}");
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["budget"]["daily_tokens"].as_integer(), Some(1000));
        assert_eq!(get(&doc, "attach.port").as_deref(), Some("7791"));
        assert_eq!(get(&doc, "model").as_deref(), Some("openai-codex/gpt-5.5"));
        assert_eq!(
            accounts(&doc),
            [("x".into(), "p/m2".into()), ("y".into(), "q/n".into())]
        );
    }

    #[test]
    fn orders_and_removes_accounts() {
        let mut doc: DocumentMut = ORIGINAL.parse().unwrap();
        upsert_account(&mut doc, "y", "q/n");
        upsert_account(&mut doc, "z", "r/o");
        assert!(order_accounts(&mut doc, &["z".into(), "x".into()]).is_err());
        order_accounts(&mut doc, &["z".into(), "x".into(), "y".into()]).unwrap();
        let names: Vec<_> = accounts(&doc).into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["z", "x", "y"]);
        let text = doc.to_string();
        let at = |n: &str| text.find(&format!("name = \"{n}\"")).unwrap();
        assert!(at("z") < at("x") && at("x") < at("y"), "{text}");
        assert!(doc.to_string().contains("# first"));
        assert!(remove_account(&mut doc, "x"));
        assert!(!remove_account(&mut doc, "x"));
        assert_eq!(accounts(&doc).len(), 2);
    }

    #[test]
    fn default_config_parses_as_zephyr_config() {
        let cfg: crate::config::Config = toml::from_str(&default_config(1000)).unwrap();
        assert_eq!(cfg.extensions.len(), EXTENSIONS.len());
        assert!(cfg.is_owner("chat", "1000"));
        let doc: DocumentMut = default_config(1000).parse().unwrap();
        assert_eq!(extension_args(&doc).len(), 2 * EXTENSIONS.len());
    }

    #[test]
    fn old_configs_gain_the_image_extensions_once() {
        let mut doc: DocumentMut =
            "rpi = \"/usr/local/bin/rpi\"\nhome = \"/zephyr\"\n[owners]\nchat = [\"1\"]\n"
                .parse()
                .unwrap();
        add_image_extensions(&mut doc);
        add_image_extensions(&mut doc);
        assert_eq!(extension_args(&doc).len(), 2 * EXTENSIONS.len());
        let cfg: crate::config::Config = toml::from_str(&doc.to_string()).unwrap();
        assert_eq!(cfg.extensions.len(), EXTENSIONS.len());
    }

    #[test]
    fn secrets_file_is_private_and_lines_are_replaced() {
        let dir = crate::test_dir("secrets");
        let path = dir.join("secrets.env");
        std::fs::write(&path, "# keep\nA=1\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        set_secret(&path, "B", "two").unwrap();
        set_secret(&path, "A", "3").unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "# keep\nA=3\nB=two\n"
        );
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(set_secret(&path, "C", "a\nD=b").is_err());
    }
}
