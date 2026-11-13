//! The owner's own commands from the TUI: `!cmd` runs one and shows its output, which the next
//! message carries to the agent; `/shell` hands the terminal to an interactive shell. Both run as
//! the agent, in its home, never as the root the TUI runs as.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// How long a `!` command may run before its process group is killed.
pub const LIMIT: Duration = Duration::from_secs(60);
/// How much of its output is kept.
const CAP: usize = 50 * 1024;

/// The command text of `!cmd`, when `text` is one.
pub fn bang(text: &str) -> Option<&str> {
    text.strip_prefix('!')
        .map(str::trim)
        .filter(|c| !c.is_empty())
}

/// `program args..` as the agent: in its home, with its HOME, without the TUI's attach token or
/// the hidden secrets, and on the desktop when there is one (as `procs::spawn` does).
pub fn agent(home: &Path, program: &str, args: &[&str]) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .current_dir(home)
        .env("HOME", home)
        .env_remove("RPI_SERVER_TOKEN");
    // `desktop::xauthority` reads HOME, which is the TUI's /tmp here, not the agent's.
    if let Ok(display) = std::env::var("ZEPHYR_DESKTOP") {
        cmd.env("DISPLAY", display)
            .env("XAUTHORITY", home.join(crate::desktop::XAUTHORITY));
    }
    crate::agent::cmd(&mut cmd);
    cmd
}

/// What a `!` command did.
pub struct Ran {
    pub command: String,
    pub output: String,
    /// `exit N`, `signal N`, `timed out after 60 s` or why it did not start.
    pub how: String,
}

impl Ran {
    /// The card: its title and body.
    pub fn card(&self) -> (String, String) {
        let mut body = self.output.trim_end().to_string();
        if !body.is_empty() {
            body.push('\n');
        }
        body.push_str(&format!("({})", self.how));
        (format!("$ {}", self.command), body)
    }
}

/// `message` with the held results in front, oldest first.
pub fn prefix(held: &[Ran], message: &str) -> String {
    let mut out = String::new();
    for r in held {
        let output = r.output.trim_end();
        // A fence longer than any backtick run in the output, so the output can't close it.
        let longest = output.split(|c| c != '`').map(str::len).max().unwrap_or(0);
        let fence = "`".repeat(longest.max(2) + 1);
        out.push_str(&format!(
            "[the owner ran `{}` ({}):\n{fence}\n{output}\n{fence}]\n\n",
            r.command, r.how
        ));
    }
    out + message
}

/// Runs `bash -c command` as the agent, stdout and stderr together, killing its process group
/// after `limit`. Blocks.
pub fn run(home: &Path, command: &str, limit: Duration) -> Ran {
    let (output, how) = match capture(agent(home, "bash", &["-c", command]), limit) {
        Ok(done) => done,
        Err(e) => (String::new(), format!("did not start: {e}")),
    };
    Ran {
        command: command.to_string(),
        output,
        how,
    }
}

fn capture(mut cmd: Command, limit: Duration) -> std::io::Result<(String, String)> {
    use std::os::unix::process::CommandExt;
    let (mut reader, writer) = std::io::pipe()?;
    cmd.stdin(Stdio::null())
        .stdout(writer.try_clone()?)
        .stderr(writer)
        .process_group(0);
    let mut child = cmd.spawn()?;
    drop(cmd); // its copies of the pipe's write end, or the read below never ends
    let pid = child.id() as i32;
    let kept = Arc::new(Mutex::new((Vec::new(), false)));
    let reading = {
        let kept = kept.clone();
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n @ 1..) = reader.read(&mut chunk) {
                let mut k = kept.lock().unwrap();
                let room = CAP - k.0.len();
                k.0.extend_from_slice(&chunk[..n.min(room)]);
                k.1 |= n > room; // the rest is drained, so the command never blocks on output
            }
        })
    };
    let start = Instant::now();
    let mut status = None;
    let mut timed_out = false;
    // Done when the command exited and nothing holds its output open, or at the limit.
    while status.is_none() || !reading.is_finished() {
        if status.is_none() {
            status = child.try_wait()?;
        }
        if start.elapsed() >= limit {
            timed_out = status.is_none();
            // SAFETY: a plain syscall on the group this child leads.
            unsafe { libc::killpg(pid, libc::SIGKILL) };
            status = Some(child.wait()?);
            let deadline = Instant::now() + Duration::from_secs(1);
            while !reading.is_finished() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(20));
            }
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let (bytes, cut) = std::mem::take(&mut *kept.lock().unwrap());
    let mut output = String::from_utf8_lossy(&bytes).into_owned();
    if cut {
        output.push_str(&format!("\n[output truncated at {} KB]", CAP / 1024));
    }
    use std::os::unix::process::ExitStatusExt;
    let status = status.expect("waited");
    let how = match (timed_out, status.code(), status.signal()) {
        (true, ..) => format!("timed out after {} s", limit.as_secs()),
        (_, Some(code), _) => format!("exit {code}"),
        (_, _, Some(sig)) => format!("signal {sig}"),
        _ => "exit ?".into(),
    };
    Ok((output, how))
}

/// An interactive `shell -l` as the agent on this terminal, until it exits. The caller has
/// handed the terminal over (input reader paused, TUI suspended).
pub fn interactive(home: &Path, shell: &str) -> std::io::Result<std::process::ExitStatus> {
    foreground(agent(home, shell, &["-l"]))
}

/// Ctrl+G: the draft in `editor` (`$VISUAL`, `$EDITOR`, a command line run by `sh`) as the agent,
/// on this terminal, handed over as for [`interactive`]. The edited text, or `None` when the
/// editor failed and the draft should stay. The file is the agent's: made new (`O_EXCL`, no
/// link followed) in a fresh 0700 folder with a random name, read back only as a regular file
/// (`files::read_capped`, so a FIFO put in its place never stalls the TUI), and gone afterwards
/// with its folder.
pub fn edit(home: &Path, editor: &str, draft: &str) -> std::io::Result<Option<String>> {
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let cache = home.join(".cache");
    let dir = cache.join(format!("zephyr-edit-{}", crate::attach::nonce().1));
    let path = dir.join("draft.md");
    {
        let _fs = crate::agent::fs();
        std::fs::create_dir_all(&cache)?;
        std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
        let mut file = (std::fs::OpenOptions::new().write(true).create_new(true))
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW)
            .open(&path)?;
        file.write_all(draft.as_bytes())?;
    }
    let script = format!("{editor} \"$1\"");
    let file = path.to_string_lossy();
    let ran = foreground(agent(home, "sh", &["-c", &script, "sh", &file]));
    let _fs = crate::agent::fs();
    let edited = super::files::read_capped(&path, DRAFT_BYTES);
    let _ = std::fs::remove_dir_all(&dir);
    match (ran?.success(), edited) {
        (true, Some(text)) => Ok(Some(text.trim_end_matches('\n').to_string())),
        _ => Ok(None),
    }
}

/// The most of an edited draft read back.
const DRAFT_BYTES: u64 = 1 << 20;

/// Runs `cmd` on the terminal until it exits. Ctrl+C and Ctrl+\ there reach the TUI too; it
/// catches them with a handler that does nothing, rather than ignoring them, because an ignored
/// signal stays ignored in the children while a caught one goes back to the default at exec.
fn foreground(mut cmd: Command) -> std::io::Result<std::process::ExitStatus> {
    extern "C" fn nothing(_: libc::c_int) {}
    let handler = nothing as extern "C" fn(libc::c_int) as libc::sighandler_t;
    // SAFETY: installs a handler that touches nothing; the old ones are put back below.
    let old = unsafe {
        [
            libc::signal(libc::SIGINT, handler),
            libc::signal(libc::SIGQUIT, handler),
        ]
    };
    let ran = cmd.status();
    unsafe {
        libc::signal(libc::SIGINT, old[0]);
        libc::signal(libc::SIGQUIT, old[1]);
    }
    ran
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;

    fn ran(command: &str, output: &str, how: &str) -> Ran {
        Ran {
            command: command.into(),
            output: output.into(),
            how: how.into(),
        }
    }

    #[test]
    fn a_bang_is_a_command_after_the_mark() {
        assert_eq!(bang("!ls -la"), Some("ls -la"));
        assert_eq!(bang("! git status "), Some("git status"));
        assert_eq!(bang("!"), None);
        assert_eq!(bang("!   "), None);
        assert_eq!(bang("ls"), None);
        assert_eq!(bang("/shell"), None);
    }

    #[test]
    fn held_results_go_in_front_of_the_message_in_order() {
        let held = [ran("ls", "a\nb\n", "exit 0"), ran("false", "", "exit 1")];
        assert_eq!(
            prefix(&held, "what now?"),
            "[the owner ran `ls` (exit 0):\n```\na\nb\n```]\n\n\
             [the owner ran `false` (exit 1):\n```\n\n```]\n\nwhat now?"
        );
        assert_eq!(prefix(&[], "hi"), "hi");
        let fenced = prefix(
            &[ran("cat x.md", "```rust\nfn x() {}\n```", "exit 0")],
            "ok",
        );
        assert!(fenced.starts_with("[the owner ran `cat x.md` (exit 0):\n````\n```rust"));
        assert!(fenced.ends_with("```\n````]\n\nok"));
    }

    #[test]
    fn the_card_shows_the_command_output_and_exit() {
        let (title, body) = ran("ls", "a\n", "exit 0").card();
        assert_eq!((title.as_str(), body.as_str()), ("$ ls", "a\n(exit 0)"));
        assert_eq!(ran("true", "", "exit 0").card().1, "(exit 0)");
    }

    #[test]
    fn the_command_runs_as_the_agent_in_its_home_without_the_token() {
        let home = std::env::temp_dir();
        let cmd = agent(&home, "bash", &["-c", "true"]);
        let envs: Vec<_> = cmd.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("HOME"), Some(home.as_os_str()))));
        assert!(envs.contains(&(OsStr::new("RPI_SERVER_TOKEN"), None)));
        assert_eq!(cmd.get_current_dir(), Some(home.as_path()));

        let r = run(&home, "echo out; echo err >&2; pwd; exit 3", LIMIT);
        let want = format!("out\nerr\n{}\n", home.canonicalize().unwrap().display());
        assert_eq!((r.output, r.how.as_str()), (want, "exit 3"));
    }

    #[test]
    fn long_output_is_cut_and_says_so() {
        let r = run(
            &std::env::temp_dir(),
            "head -c 200000 /dev/zero | tr '\\0' x",
            LIMIT,
        );
        assert_eq!(r.how, "exit 0");
        assert!(r.output.starts_with(&"x".repeat(CAP)));
        assert!(
            r.output.ends_with("\n[output truncated at 50 KB]"),
            "{}",
            r.output.len()
        );
        assert_eq!(r.output.len(), CAP + "\n[output truncated at 50 KB]".len());
    }

    #[test]
    fn ctrl_g_edits_the_draft_in_the_editor_and_a_failed_editor_keeps_it() {
        let home = crate::test_dir("shell-edit");
        let script = home.join("ed.sh");
        std::fs::write(&script, "printf ' and more\\n' >> \"$1\"\n").unwrap();
        let editor = format!("sh {}", script.display());
        assert_eq!(
            edit(&home, &editor, "the draft").unwrap().as_deref(),
            Some("the draft and more")
        );
        assert_eq!(edit(&home, "false", "kept").unwrap(), None);
        // The file goes either way.
        let left = std::fs::read_dir(home.join(".cache")).unwrap().count();
        assert_eq!(left, 0);
    }

    /// The draft's file is the agent's to write, so what the TUI reads back is only a regular
    /// file, read without waiting: an editor (or anything of the agent's) that puts a FIFO in
    /// its place keeps the draft as it was, and never stalls the TUI.
    #[test]
    fn a_draft_file_turned_into_a_fifo_never_stalls_the_tui() {
        let home = crate::test_dir("shell-edit-fifo");
        let script = home.join("ed.sh");
        std::fs::write(&script, "rm -f \"$1\" && mkfifo \"$1\"\n").unwrap();
        let editor = format!("sh {}", script.display());
        let (tx, rx) = std::sync::mpsc::channel();
        let at = home.clone();
        std::thread::spawn(move || {
            let _ = tx.send(edit(&at, &editor, "the draft").map_err(|e| e.to_string()));
        });
        let got = rx
            .recv_timeout(Duration::from_secs(5))
            .expect("Ctrl+G blocked on a FIFO");
        assert_eq!(got, Ok(None), "the draft stays as it was");
        let left = std::fs::read_dir(home.join(".cache")).unwrap().count();
        assert_eq!(left, 0);
    }

    #[test]
    fn a_command_past_the_limit_is_killed_with_its_children() {
        let started = Instant::now();
        let r = run(
            &std::env::temp_dir(),
            "echo before; sleep 30 & sleep 30",
            Duration::from_millis(300),
        );
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(r.how, "timed out after 0 s");
        assert_eq!(r.output, "before\n");
    }
}
