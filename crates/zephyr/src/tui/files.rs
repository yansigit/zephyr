//! Files from the composer (DESIGN §4.3.2): `@path` references with completion, images sent
//! as the same attachment line a Discord photo gets, a dragged-in path, and the owner's saved
//! prompts in `/zephyr/prompts/`. The TUI runs as root, so every file is looked at inside
//! `agent::fs`: it sees what the agent could, and a link the agent planted leads nowhere else.
//!
//! A path with spaces is written with `\ ` (as a shell would), so a token is one word.

use super::overlays::Choice;
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Never listed: big, and never what you meant.
const SKIP: [&str; 3] = [".git", "target", "node_modules"];
/// The most completions listed.
const CAP: usize = 50;
/// The most entries of one folder read for a completion: the agent may fill one.
const ENTRIES: usize = 4096;
/// The most of a saved prompt read, and of one read for its first line only.
const PROMPT_BYTES: u64 = 64 * 1024;
const FIRST_LINE_BYTES: u64 = 4 * 1024;

/// The first `cap` bytes of `path`, a file in a folder the agent writes, or `None` when it is
/// not a regular file: opened without following a link or waiting on a FIFO's writer, and
/// checked on the open descriptor, so a FIFO cannot hang the UI loop nor a sparse 20 GB file
/// fill its memory.
fn read_capped(path: &Path, cap: u64) -> Option<String> {
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let file = (std::fs::OpenOptions::new().read(true))
        .custom_flags(libc::O_NONBLOCK | libc::O_NOFOLLOW)
        .open(path)
        .ok()?;
    if !file.metadata().ok()?.is_file() {
        return None;
    }
    let mut bytes = Vec::new();
    file.take(cap).read_to_end(&mut bytes).ok()?;
    // The agent's text, bound for the composer and the `/p` card: cleaned as `conn` cleans.
    Some(super::view::safe(&String::from_utf8_lossy(&bytes)))
}

/// The words of `text`, split at whitespace that is not escaped with `\`.
fn words(text: &str) -> Vec<Range<usize>> {
    let mut out = vec![];
    let mut start = None;
    let mut escaped = false;
    for (i, c) in text.char_indices() {
        match (c.is_whitespace() && !escaped, start) {
            (true, Some(s)) => {
                out.push(s..i);
                start = None;
            }
            (false, None) => start = Some(i),
            _ => {}
        }
        escaped = c == '\\' && !escaped;
    }
    out.extend(start.map(|s| s..text.len()));
    out
}

fn escape(name: &str) -> String {
    name.replace(' ', "\\ ")
}

fn unescape(token: &str) -> String {
    token.replace("\\ ", " ")
}

/// The partial path of the `@` word that ends where `before` (the text up to the cursor) ends.
pub fn token(before: &str) -> Option<&str> {
    let last = words(before).pop().filter(|r| r.end == before.len())?;
    before[last].strip_prefix('@')
}

/// Where a token points: `~/x` in the agent's home, `/x` as written, anything else in the
/// projects folder.
fn resolve(partial: &str, home: &Path, projects: Option<&Path>) -> Option<PathBuf> {
    let partial = unescape(partial);
    match partial.strip_prefix('~') {
        Some(rest) if rest.is_empty() || rest.starts_with('/') => {
            Some(home.join(rest.trim_start_matches('/')))
        }
        _ if partial.starts_with('/') => Some(PathBuf::from(partial)),
        _ => Some(projects?.join(partial)),
    }
}

/// The `@` completions for the text up to the cursor: the entries of the folder typed so far
/// whose names start with the rest, folders first. None once it names a file: Enter sends.
pub fn complete(before: &str, home: &Path, projects: Option<&Path>) -> Vec<Choice> {
    let Some(partial) = token(before) else {
        return vec![];
    };
    let named = resolve(partial, home, projects).is_some_and(|p| {
        let _fs = crate::agent::fs();
        p.is_file()
    });
    if named {
        return vec![];
    }
    let (dir, prefix) = match partial.rfind('/') {
        Some(i) => partial.split_at(i + 1),
        None => ("", partial),
    };
    let Some(base) = resolve(dir, home, projects) else {
        return vec![];
    };
    let prefix = unescape(prefix).to_lowercase();
    let mut found: Vec<(bool, String)> = {
        let _fs = crate::agent::fs();
        let Ok(list) = std::fs::read_dir(&base) else {
            return vec![];
        };
        list.flatten()
            .take(ENTRIES)
            .filter_map(|e| {
                // A name holding an escape or a control is not listed: it would reach the terminal.
                let name =
                    (e.file_name().into_string().ok()).filter(|n| super::view::safe(n) == *n)?;
                let hidden = name.starts_with('.') && !prefix.starts_with('.');
                let wanted = name.to_lowercase().starts_with(&prefix);
                (wanted && !hidden && !SKIP.contains(&name.as_str()))
                    .then(|| (!e.path().is_dir(), name))
            })
            .collect()
    };
    found.sort_by_key(|(file, name)| (*file, name.to_lowercase()));
    let head = &before[..before.len() - partial.len()];
    (found.into_iter().take(CAP))
        .map(|(file, name)| Choice {
            label: if file {
                name.clone()
            } else {
                format!("{name}/")
            },
            detail: String::new(),
            insert: format!(
                "{head}{dir}{}{}",
                escape(&name),
                if file { " " } else { "/" }
            ),
            then_run: false,
        })
        .collect()
}

/// The message as sent: each `@token` naming a file that exists becomes `@<absolute path>`,
/// and each image among them adds the attachment line rpi's `read` is pointed at.
pub fn expand(text: &str, home: &Path, projects: Option<&Path>) -> String {
    let mut out = String::new();
    let mut lines = vec![];
    let mut at = 0;
    for w in words(text) {
        let found = (text[w.clone()].strip_prefix('@'))
            .filter(|p| !p.is_empty())
            .and_then(|p| resolve(p, home, projects))
            .and_then(|path| {
                let _fs = crate::agent::fs();
                Some((std::fs::metadata(&path).ok()?, path))
            });
        let Some((meta, path)) = found else {
            continue;
        };
        out.push_str(&text[at..w.start]);
        out.push_str(&format!("@{}", escape(&path.to_string_lossy())));
        at = w.end;
        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
        let ext = ext.to_ascii_lowercase();
        if meta.is_file() && crate::inbox::IMAGES.contains(&ext.as_str()) {
            let mime = match ext.as_str() {
                "jpg" | "jpeg" => "image/jpeg".to_string(),
                other => format!("image/{other}"),
            };
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            let line = crate::inbox::attached_image_line(&path, &name, &mime, meta.len());
            lines.push(line);
        }
    }
    out.push_str(&text[at..]);
    for line in lines {
        out.push('\n');
        out.push_str(&line);
    }
    out
}

/// A paste that is one path dragged onto the terminal: `Some(Ok(@path ))` to insert for one in
/// the projects folder or the agent's home, `Some(Err(hint))` for one the container cannot see,
/// `None` for anything else, which is pasted as it came.
pub fn dragged(text: &str, home: &Path, projects: Option<&Path>) -> Option<Result<String, String>> {
    let raw = text.trim();
    let quoted = raw.len() >= 2
        && ['\'', '"']
            .iter()
            .any(|&q| raw.starts_with(q) && raw.ends_with(q));
    let inner = if quoted { &raw[1..raw.len() - 1] } else { raw };
    // Terminals escape a dragged path's spaces; an unescaped space is text, not a path.
    if raw.contains('\n') || (!quoted && words(inner).len() != 1) {
        return None;
    }
    let mut path = String::new();
    let mut chars = inner.chars();
    while let Some(c) = chars.next() {
        path.push(if c == '\\' { chars.next()? } else { c });
    }
    let path = PathBuf::from(path);
    // `/model x` or `/stop` pasted is not a path: a dragged one has a folder in it.
    if !path.is_absolute() || path.components().count() < 3 {
        return None;
    }
    let exists = {
        let _fs = crate::agent::fs();
        path.exists()
    };
    let seen = projects.unwrap_or(home);
    match exists {
        true if path.starts_with(home) || projects.is_some_and(|p| path.starts_with(p)) => {
            Some(Ok(format!("@{} ", escape(&path.to_string_lossy()))))
        }
        true => None,
        false => Some(Err(format!(
            "{} is not in the container: only files under {} can be seen",
            path.display(),
            seen.display()
        ))),
    }
}

/// The owner's saved prompts, `<home>/prompts/<name>.md`, by name, each with its first line.
pub fn prompts(home: &Path) -> Vec<(String, String)> {
    let _fs = crate::agent::fs();
    let Ok(list) = std::fs::read_dir(home.join("prompts")) else {
        return vec![];
    };
    let mut out: Vec<(String, String)> = (list.flatten())
        .filter_map(|e| {
            let name = (e.file_name().into_string().ok()).filter(|n| super::view::safe(n) == *n)?;
            let name = name.strip_suffix(".md").filter(|n| !n.starts_with('.'))?;
            let text = read_capped(&e.path(), FIRST_LINE_BYTES)?;
            let first = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
            Some((name.to_string(), first.trim().to_string()))
        })
        .collect();
    out.sort();
    out
}

/// `/p`'s card: each saved prompt with its first line, or where to put one.
pub fn listing(home: &Path) -> String {
    let list = prompts(home);
    if list.is_empty() {
        let dir = home.join("prompts");
        return format!(
            "no saved prompts yet: each is a markdown file, {}/<name>.md, \
             with $ARGUMENTS or $1..$9 for what follows /p <name>; the agent can write one",
            dir.display()
        );
    }
    let w = list.iter().map(|(n, _)| n.len()).max().unwrap_or(0);
    let rows: Vec<String> = (list.iter())
        .map(|(name, first)| format!("{name:<w$}  {first}"))
        .collect();
    rows.join("\n")
}

/// `/p name args`: the template with `$ARGUMENTS` and `$1`..`$9` filled in, for the composer.
pub fn prompt(home: &Path, name: &str, args: &str) -> Result<String, String> {
    let bad = name.is_empty() || name.starts_with('.') || name.contains('/');
    let path = home.join("prompts").join(format!("{name}.md"));
    let text = match bad {
        true => None,
        false => {
            let _fs = crate::agent::fs();
            read_capped(&path, PROMPT_BYTES)
        }
    };
    let text = text.ok_or_else(|| format!("no saved prompt {name}; /p lists them"))?;
    Ok(fill(text.trim(), args))
}

fn fill(template: &str, args: &str) -> String {
    let words: Vec<&str> = args.split_whitespace().collect();
    let mut out = template.replace("$ARGUMENTS", args.trim());
    for n in (1..=9).rev() {
        out = out.replace(&format!("${n}"), words.get(n - 1).copied().unwrap_or(""));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree(name: &str) -> (PathBuf, PathBuf) {
        let root = crate::test_dir(name);
        let (home, projects) = (root.join("zephyr"), root.join("projects"));
        for d in [
            "app/src",
            "Apple",
            ".hidden",
            ".git",
            "target",
            "node_modules",
            "my dir",
        ] {
            std::fs::create_dir_all(projects.join(d)).unwrap();
        }
        for f in [
            "app/README.md",
            "about.txt",
            "my dir/shot one.png",
            "pic.PNG",
        ] {
            std::fs::write(projects.join(f), "x").unwrap();
        }
        std::fs::create_dir_all(home.join("notes")).unwrap();
        (home, projects)
    }

    fn labels(c: Vec<Choice>) -> Vec<String> {
        c.into_iter().map(|c| c.label).collect()
    }

    #[test]
    fn a_token_is_an_at_word_ending_at_the_cursor() {
        assert_eq!(token("see @src/ma"), Some("src/ma"));
        assert_eq!(token("@"), Some(""));
        assert_eq!(token("@my\\ dir/sh"), Some("my\\ dir/sh"));
        // An address is not a reference, and a finished word is not being typed.
        assert_eq!(token("mail me@example.com"), None);
        assert_eq!(token("@src "), None);
        assert_eq!(token("plain"), None);
    }

    #[test]
    fn completion_lists_folders_first_and_skips_the_noise() {
        let (home, projects) = tree("files-complete");
        let p = Some(projects.as_path());
        assert_eq!(
            labels(complete("look at @a", &home, p)),
            ["app/", "Apple/", "about.txt"]
        );
        // Every listing skips dot entries, .git, target and node_modules...
        let all = labels(complete("@", &home, p));
        assert_eq!(all, ["app/", "Apple/", "my dir/", "about.txt", "pic.PNG"]);
        // ...except dot entries asked for by name.
        assert_eq!(labels(complete("@.h", &home, p)), [".hidden/"]);
        let c = &complete("x @app/R", &home, p)[0];
        assert_eq!(
            (c.insert.as_str(), c.then_run),
            ("x @app/README.md ", false)
        );
        assert_eq!(complete("@ap", &home, p)[0].insert, "@app/");
        assert_eq!(complete("@my", &home, p)[0].insert, "@my\\ dir/");
        assert_eq!(labels(complete("@my\\ dir/", &home, p)), ["shot one.png"]);
        assert_eq!(labels(complete("@~/n", &home, p)), ["notes/"]);
        assert!(complete("@/nonexistent/x", &home, p).is_empty());
        // A whole file name is done: Enter sends rather than adding a space.
        assert!(complete("@about.txt", &home, p).is_empty());
        assert!(complete("me@a", &home, p).is_empty());
        // No projects folder: only `~/` and absolute paths complete.
        assert!(complete("@a", &home, None).is_empty());

        for i in 0..60 {
            std::fs::write(projects.join(format!("many{i:02}")), "").unwrap();
        }
        assert_eq!(complete("@many", &home, p).len(), CAP);
    }

    #[test]
    fn sending_makes_references_absolute_and_attaches_images() {
        let (home, projects) = tree("files-expand");
        let p = Some(projects.as_path());
        let shot = projects.join("my dir/shot one.png");
        let png = projects.join("pic.PNG");
        let got = expand(
            "compare @my\\ dir/shot\\ one.png and\n@pic.PNG, not @nope or a@b.c, in @app",
            &home,
            p,
        );
        let line =
            |path: &Path, name: &str| crate::inbox::attached_image_line(path, name, "image/png", 1);
        // `@pic.PNG,` is not a file: the comma is part of the word.
        let at = projects.display();
        assert_eq!(
            got,
            format!(
                "compare @{at}/my\\ dir/shot\\ one.png and\n@pic.PNG, not @nope or a@b.c, in @{at}/app\n{}",
                line(&shot, "shot one.png"),
            )
        );
        assert_eq!(
            expand("@pic.PNG", &home, p),
            format!("@{}\n{}", png.display(), line(&png, "pic.PNG"))
        );
        // The line is the one a Discord photo gets.
        assert_eq!(
            line(&png, "pic.PNG"),
            format!(
                "[attached image: {} (pic.PNG, image/png, 1 KB); open it with read]",
                png.display()
            )
        );
        assert_eq!(expand("no refs here", &home, p), "no refs here");
    }

    #[test]
    fn a_dragged_path_becomes_a_reference_or_a_hint() {
        let (home, projects) = tree("files-drag");
        let p = Some(projects.as_path());
        let shot = projects.join("my dir/shot one.png");
        let escaped = shot.display().to_string().replace(' ', "\\ ");
        let want = Some(Ok(format!("@{escaped} ")));
        assert_eq!(dragged(&escaped, &home, p), want);
        assert_eq!(dragged(&format!("'{}'", shot.display()), &home, p), want);
        assert_eq!(
            dragged(&format!(" \"{}\"\n", shot.display()), &home, p),
            want
        );
        assert_eq!(
            dragged("/Users/me/Desktop/shot.png", &home, p),
            Some(Err(format!(
                "/Users/me/Desktop/shot.png is not in the container: only files under {} can be seen",
                projects.display()
            )))
        );
        // Text, a command, or a file the agent has no business with are pasted as they are.
        for text in [
            "hello there",
            "/model a/b",
            "/stop",
            "a\nb",
            "/etc/passwd",
            "x/y/z",
        ] {
            assert_eq!(dragged(text, &home, p), None, "{text}");
        }
    }

    #[test]
    fn saved_prompts_are_listed_and_filled_in() {
        let home = crate::test_dir("files-prompts");
        let dir = home.join("prompts");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("review.md"),
            "\nReview $1 for $2.\nAll: $ARGUMENTS\n",
        )
        .unwrap();
        std::fs::write(dir.join("hi.md"), "Say hi").unwrap();
        std::fs::write(dir.join("notes.txt"), "not a prompt").unwrap();
        assert_eq!(
            prompts(&home),
            [
                ("hi".to_string(), "Say hi".to_string()),
                ("review".to_string(), "Review $1 for $2.".to_string())
            ]
        );
        assert_eq!(
            prompt(&home, "review", " main.rs bugs "),
            Ok("Review main.rs for bugs.\nAll: main.rs bugs".into())
        );
        assert_eq!(
            prompt(&home, "review", ""),
            Ok("Review  for .\nAll: ".into())
        );
        for bad in ["nope", "../prompts/hi", ""] {
            assert!(prompt(&home, bad, "").is_err(), "{bad}");
        }
    }

    /// The agent writes the prompts folder: a FIFO there never blocks the UI loop (it is
    /// skipped), and a huge file is read only as far as the cap.
    #[test]
    fn a_fifo_or_a_huge_file_in_the_prompts_folder_never_stalls_the_ui() {
        let home = crate::test_dir("files-prompts-fifo");
        let dir = home.join("prompts");
        std::fs::create_dir_all(&dir).unwrap();
        let fifo = std::ffi::CString::new(dir.join("x.md").to_str().unwrap()).unwrap();
        // Safety: a valid C string; the result is checked.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o644) }, 0);
        let big = std::fs::File::create(dir.join("big.md")).unwrap();
        big.set_len(100 << 20).unwrap(); // sparse: 100 MB of zeros, no disk
        std::fs::write(dir.join("ok.md"), "Fine").unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let at = home.clone();
        std::thread::spawn(move || {
            let listed = prompts(&at);
            let one = prompt(&at, "x", "");
            let huge = prompt(&at, "big", "").map(|t| t.len());
            let _ = tx.send((listed, one, huge));
        });
        let (listed, one, huge) = rx
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("prompts() blocked on a FIFO");
        let names: Vec<&str> = listed.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, ["big", "ok"], "the FIFO is no prompt");
        assert!(one.is_err());
        assert!(huge.is_ok_and(|n| n <= 64 * 1024));
    }
}
