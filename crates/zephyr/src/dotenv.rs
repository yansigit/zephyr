//! `<home>/secrets.env`: read by the daemon at startup, so a key change needs only a restart.

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

/// Sets each variable from `path` that is not already set. A missing file is not an error.
pub fn load(path: &Path) -> anyhow::Result<()> {
    let text = match std::fs::read_to_string(path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        r => r.map_err(|e| anyhow::anyhow!("reading {}: {e}", path.display()))?,
    };
    for (key, value) in parse(&text) {
        if std::env::var_os(&key).is_none() {
            // Runs before any task reads the environment.
            std::env::set_var(key, value);
        }
    }
    Ok(())
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
        load(&path).unwrap();
        assert_eq!(std::env::var("ZEPHYR_DOTENV_T1").unwrap(), "from-env");
        assert_eq!(std::env::var("ZEPHYR_DOTENV_T2").unwrap(), "from-file");
        load(&dir.join("missing.env")).unwrap();
    }
}
