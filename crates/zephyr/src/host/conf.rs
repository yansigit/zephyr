//! The files under `~/.zephyr` (the agent's home) and `~/.zephyr-daemon` (the daemon's own):
//! `zephyr.toml` edited with toml_edit (comments survive), `secrets.env` and `tui.token`, and
//! `host.toml` (host-only settings), all mode 0600.

use anyhow::{bail, Context};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use toml_edit::{value, ArrayOfTables, DocumentMut, Item, Table};

/// Extensions baked into the image (deploy/Dockerfile): the providers, the MCP adapter's tools, and
/// the two packages' cdylibs. This is also the order `zephyr.toml` lists them in.
pub const EXTENSIONS: [&str; 4] = [
    "/opt/zephyr/ext/librpi_providers.so",
    "/opt/zephyr/ext/librpi_mcp_adapter.so",
    "/opt/zephyr/ext/rpi-cursor-lite/librpi_cursor_lite.so",
    "/opt/zephyr/ext/rpi-claude-agent/librpi_claude_agent.so",
];

/// The daemon's files, which `__file` may read and write. In the container they live in
/// `/var/lib/zephyr` (on the host `~/.zephyr-daemon`), which only root can enter.
pub const DAEMON_FILES: [&str; 4] = ["zephyr.toml", "secrets.env", "tui.token", "host.toml"];

/// Replaces `path` whole with `text`, mode 0600: written beside it, then renamed over it, so a
/// reader (the daemon's 5 s reload check) never sees half a file. The temp name is unique per
/// write, so two writers cannot rename the file one of them is still filling.
pub fn replace_private(path: &Path, text: &str) -> anyhow::Result<()> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let name = path.file_name().context("no file name")?.to_string_lossy();
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let tmp = path.with_file_name(format!(".{name}.{}-{seq}.tmp", std::process::id()));
    write_private(&tmp, text)
        .and_then(|()| Ok(std::fs::rename(&tmp, path)?))
        .with_context(|| format!("writing {}", path.display()))
}

/// Where `~/.zephyr-daemon` is mounted in the container.
pub const DAEMON_MOUNT: &str = "/var/lib/zephyr";

pub struct Data {
    /// `~/.zephyr`: the agent's home, `/zephyr` in the container (sessions, memory, accounts).
    pub dir: PathBuf,
    /// `~/.zephyr-daemon`: the daemon's files (`DAEMON_FILES`, `state.db`), `DAEMON_MOUNT` in the
    /// container. Root's once `zephyr up` has run, so this user then reaches them through
    /// `zephyr __file` in the container (DESIGN §4.4).
    pub daemon: PathBuf,
    pub container: String,
    pub image: String,
}

impl Data {
    pub fn account(&self, name: &str) -> PathBuf {
        self.dir.join("accounts").join(name)
    }

    /// Whether this user can reach the daemon's files itself: before the first `zephyr up`.
    pub fn direct(&self) -> bool {
        !matches!(std::fs::read_dir(&self.daemon), Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied)
    }

    /// A daemon file's text. `NotFound` when there is none.
    pub fn read(&self, name: &str) -> std::io::Result<String> {
        if self.direct() {
            return std::fs::read_to_string(self.daemon.join(name));
        }
        let out = self
            .via("get", name)
            .stdin(std::process::Stdio::null())
            .output()?;
        match out.status.code() {
            Some(0) => Ok(String::from_utf8_lossy(&out.stdout).into_owned()),
            Some(3) => Err(std::io::ErrorKind::NotFound.into()),
            _ => Err(std::io::Error::other(format!(
                "reading {name} in the container: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }

    /// Replaces a daemon file whole, mode 0600.
    pub fn write(&self, name: &str, text: &str) -> anyhow::Result<()> {
        if self.direct() {
            std::fs::create_dir_all(&self.daemon)?;
            std::fs::set_permissions(&self.daemon, std::fs::Permissions::from_mode(0o700))?;
            return replace_private(&self.daemon.join(name), text);
        }
        let mut child = self
            .via("put", name)
            .stdin(std::process::Stdio::piped())
            .spawn()
            .context("running docker")?;
        child
            .stdin
            .take()
            .context("stdin")?
            .write_all(text.as_bytes())?;
        if !child.wait()?.success() {
            bail!("writing {name} in the container failed");
        }
        Ok(())
    }

    /// `zephyr __file OP NAME` as root in the container, or in a throwaway one when it is not
    /// running: root without capabilities, which reaches only root's own files.
    fn via(&self, op: &str, name: &str) -> std::process::Command {
        let mut cmd = std::process::Command::new("docker");
        if super::state(&self.container).as_deref() == Some("running") {
            cmd.args(["exec", "-i", "-u", "0", &self.container]);
        } else {
            let mount = format!("type=bind,src={},dst={DAEMON_MOUNT}", self.daemon.display());
            let config = format!("ZEPHYR_CONFIG={DAEMON_MOUNT}/zephyr.toml");
            cmd.args(["run", "--rm", "-i", "-u", "0", "--cap-drop", "ALL"])
                .args(["--security-opt", "no-new-privileges", "--network", "none"])
                .args(["--mount", &mount, "-e", &config, &self.image]);
        }
        cmd.args(["zephyr", "__file", op, name]);
        cmd
    }

    /// Creates what is missing: the agent's dirs (0700), and the daemon's `zephyr.toml`,
    /// `tui.token` and `secrets.env`.
    pub fn ensure_layout(&self) -> anyhow::Result<()> {
        for dir in [
            &self.dir,
            &self.dir.join(".rpi/agent"),
            &self.dir.join("accounts"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        let missing =
            |name| matches!(self.read(name), Err(e) if e.kind() == std::io::ErrorKind::NotFound);
        if missing("zephyr.toml") {
            self.write("zephyr.toml", &default_config())?;
        }
        if missing("tui.token") {
            self.write("tui.token", &random_hex(24)?)?;
        }
        if missing("secrets.env") {
            self.write(
                "secrets.env",
                "# KEY=value lines; zephyr picks up a change by itself\n",
            )?;
        }
        Ok(())
    }

    pub fn load(&self) -> anyhow::Result<DocumentMut> {
        let text = self
            .read("zephyr.toml")
            .context("reading zephyr.toml (run `zephyr init` first)")?;
        text.parse().context("parsing zephyr.toml")
    }

    pub fn save(&self, doc: &DocumentMut) -> anyhow::Result<()> {
        self.write("zephyr.toml", &doc.to_string())
    }

    /// The `zephyr attach` token.
    pub fn token(&self) -> anyhow::Result<String> {
        Ok(self.read("tui.token")?.trim().to_string())
    }

    /// Sets `key` in `secrets.env`, replacing its line or appending one.
    pub fn set_secret(&self, key: &str, secret: &str) -> anyhow::Result<()> {
        let text = match self.read("secrets.env") {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            r => r?,
        };
        self.write("secrets.env", &with_secret(&text, key, secret)?)
    }

    /// `projects` from `host.toml`.
    pub fn projects(&self) -> anyhow::Result<Option<PathBuf>> {
        Ok(self.host_get("projects")?.map(PathBuf::from))
    }

    /// A key of `host.toml`, e.g. `memory` (the container's `--memory` limit).
    pub fn host_get(&self, key: &str) -> anyhow::Result<Option<String>> {
        Ok(self.host_doc()?.and_then(|doc| get(&doc, key)))
    }

    pub fn set_projects(&self, path: &Path) -> anyhow::Result<()> {
        let mut doc = self.host_doc()?.unwrap_or_default();
        doc["projects"] = value(path.display().to_string());
        self.write("host.toml", &doc.to_string())
    }

    fn host_doc(&self) -> anyhow::Result<Option<DocumentMut>> {
        match self.read("host.toml") {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            r => Ok(Some(r?.parse().context("parsing host.toml")?)),
        }
    }
}

/// Whole or not at all: the running daemon reloads the file when its mtime changes.
pub fn save(path: &Path, doc: &DocumentMut) -> anyhow::Result<()> {
    replace_private(path, &doc.to_string())
}

/// A read-modify-write of `zephyr.toml`, one writer at a time: the daemon writes it from `/model`
/// and from a `/login` that records an account, and two of those interleaved would keep only the
/// later row. The lock is per process; the host CLI edits the same file from outside, which the
/// 5 s reload picks up (the temp file it writes is unique, so neither ever reads half a file).
pub fn update<F>(path: &Path, f: F) -> anyhow::Result<()>
where
    F: FnOnce(&mut DocumentMut) -> anyhow::Result<()>,
{
    static WRITING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _guard = WRITING.lock().unwrap_or_else(|e| e.into_inner());
    let mut doc = load(path)?;
    f(&mut doc)?;
    save(path, &doc)
}

pub fn load(path: &Path) -> anyhow::Result<DocumentMut> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {} (run `zephyr init` first)", path.display()))?;
    text.parse()
        .with_context(|| format!("parsing {}", path.display()))
}

/// `owners.chat` is root: the host's `zephyr chat` runs as root in the container, and the agent's
/// uid cannot reach the socket at all.
fn default_config() -> String {
    let ext = EXTENSIONS.map(|e| format!("\"{e}\"")).join(", ");
    format!(
        r#"# Zephyr's config, seen as {DAEMON_MOUNT}/zephyr.toml in the container. Edit with `zephyr config set`.
rpi = "/usr/local/bin/rpi"
home = "/zephyr"
state = "{DAEMON_MOUNT}"
agent_dir = "/zephyr/.rpi/agent"
policy_plugin = "/usr/local/lib/libzephyr_policy.so"
extensions = [{ext}]
# model = "provider/model-id"   # `zephyr model` picks one

[owners]
chat = ["0"]

[attach]
port = 7790
token_file = "{DAEMON_MOUNT}/tui.token"
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

/// `text` (a dotenv file) with `key` set, its line replaced or one appended.
pub fn with_secret(text: &str, key: &str, secret: &str) -> anyhow::Result<String> {
    if secret.contains(['\n', '\r']) {
        bail!("{key}: the value must be one line");
    }
    let line = format!("{key}={secret}");
    let mut lines: Vec<&str> = text.lines().collect();
    match lines
        .iter()
        .position(|l| l.split_once('=').is_some_and(|(k, _)| k.trim() == key))
    {
        Some(i) => lines[i] = &line,
        None => lines.push(&line),
    }
    Ok(lines.join("\n") + "\n")
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

/// Adds the image's extensions a config lacks (one written before they were baked in), keeping the
/// list in `EXTENSIONS` order so an upgraded config matches a fresh one.
pub fn add_image_extensions(doc: &mut DocumentMut) {
    let list = doc
        .entry("extensions")
        .or_insert_with(|| value(toml_edit::Array::new()));
    let Some(list) = list.as_array_mut() else {
        return;
    };
    if EXTENSIONS
        .iter()
        .all(|e| list.iter().any(|v| v.as_str() == Some(e)))
    {
        return;
    }
    let mut entries: Vec<toml_edit::Value> = list.iter().cloned().collect();
    for e in EXTENSIONS {
        if !entries.iter().any(|v| v.as_str() == Some(e)) {
            entries.push(toml_edit::Value::from(e));
        }
    }
    // Stable: the image's paths sort into EXTENSIONS order, any the owner added keep their order
    // after them.
    let rank = |v: &toml_edit::Value| {
        v.as_str()
            .and_then(|s| EXTENSIONS.iter().position(|e| *e == s))
            .unwrap_or(EXTENSIONS.len())
    };
    entries.sort_by_key(rank);
    list.clear();
    for v in entries {
        list.push(v);
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
        let cfg: crate::config::Config = toml::from_str(&default_config()).unwrap();
        assert_eq!(cfg.extensions.len(), EXTENSIONS.len());
        assert!(cfg.is_owner("chat", "0"));
        assert_eq!(cfg.state_dir(), Path::new(DAEMON_MOUNT));
        let doc: DocumentMut = default_config().parse().unwrap();
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
    fn image_extensions_are_added_in_canonical_order() {
        let mut doc: DocumentMut = format!(
            "extensions = [\"{}\", \"{}\", \"/opt/mine.so\"]\n",
            EXTENSIONS[2], EXTENSIONS[0]
        )
        .parse()
        .unwrap();
        add_image_extensions(&mut doc);
        let list = doc.get("extensions").and_then(Item::as_array).unwrap();
        let paths: Vec<_> = list.iter().filter_map(|v| v.as_str()).collect();
        let mut want: Vec<&str> = EXTENSIONS.to_vec();
        want.push("/opt/mine.so");
        assert_eq!(paths, want);
    }

    #[test]
    fn secret_lines_are_replaced_or_appended() {
        let text = with_secret("# keep\nA=1\n", "B", "two").unwrap();
        let text = with_secret(&text, "A", "3").unwrap();
        assert_eq!(text, "# keep\nA=3\nB=two\n");
        assert!(with_secret(&text, "C", "a\nD=b").is_err());
    }

    #[test]
    fn daemon_files_are_replaced_whole_and_private() {
        let dir = crate::test_dir("replace");
        let path = dir.join("secrets.env");
        std::fs::write(&path, "old").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        replace_private(&path, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
        assert!(!dir.join(".secrets.env.tmp").exists());
    }
}
