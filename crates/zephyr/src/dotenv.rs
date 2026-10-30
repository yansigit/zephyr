//! `<home>/secrets.env`: read by the daemon at startup. A change re-execs the daemon (see `reload`).

use std::path::Path;

/// `KEY=value` lines. Blank lines and `#` comments are skipped; one pair of matching quotes is stripped.
pub fn parse(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim();
            if line.starts_with('#') {
                return None;
            }
            let (key, value) = line.split_once('=')?;
            let key = key.trim();
            let value = value.trim();
            let value = match value.as_bytes() {
                [q @ (b'"' | b'\''), .., last] if last == q => &value[1..value.len() - 1],
                _ => value,
            };
            (!key.is_empty()).then(|| (key.to_string(), value.to_string()))
        })
        .collect()
}

/// The pairs in `path`; none when there is no file.
pub fn read(path: &Path) -> anyhow::Result<Vec<(String, String)>> {
    match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        r => Ok(parse(&r.map_err(|e| {
            anyhow::anyhow!("reading {}: {e}", path.display())
        })?)),
    }
}

/// Sets each variable from `path` that is not already set, and returns the names it set.
/// A missing file is not an error.
pub fn load(path: &Path) -> anyhow::Result<Vec<String>> {
    let mut set = Vec::new();
    for (key, value) in read(path)? {
        if std::env::var_os(&key).is_none() {
            // Runs before any task reads the environment.
            std::env::set_var(&key, value);
            set.push(key);
        }
    }
    Ok(set)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_pairs_comments_and_quotes() {
        let got = parse(
            "# comment\n\nA=1\n  B = two words \nC=\"quoted # not a comment\"\nD='x'\nE=\nF=a=b\nnoequals\n=novalue\nG=\"unbalanced\n",
        );
        let want = [
            ("A", "1"),
            ("B", "two words"),
            ("C", "quoted # not a comment"),
            ("D", "x"),
            ("E", ""),
            ("F", "a=b"),
            ("G", "\"unbalanced"),
        ];
        let want: Vec<_> = want
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        assert_eq!(got, want);
    }

    #[test]
    fn load_keeps_variables_already_set() {
        let dir = crate::test_dir("dotenv");
        let path = dir.join("secrets.env");
        std::fs::write(
            &path,
            "ZEPHYR_DOTENV_T1=from-file\nZEPHYR_DOTENV_T2=from-file\n",
        )
        .unwrap();
        std::env::set_var("ZEPHYR_DOTENV_T1", "from-env");
        assert_eq!(load(&path).unwrap(), ["ZEPHYR_DOTENV_T2"]);
        assert_eq!(std::env::var("ZEPHYR_DOTENV_T1").unwrap(), "from-env");
        assert_eq!(std::env::var("ZEPHYR_DOTENV_T2").unwrap(), "from-file");
        load(&dir.join("missing.env")).unwrap();
    }
}
