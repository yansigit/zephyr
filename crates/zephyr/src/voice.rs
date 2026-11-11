//! Local voice (DESIGN §4.3): an owner's voice note is transcribed by whisper.cpp. ffmpeg and
//! whisper-cli are parsers fed by files people send, so they run from the image's root-owned
//! [`DIR`] as `nobody`: not root, whose files an exploit would reach, and not the agent, which
//! could ptrace them and rewrite the text. They read sealed memfds, never a path the agent can
//! write, and the floor gives uid 65534 no network (`egress::ruleset`).
//!
//! The real binaries' argv and output shapes are in `decode_args`, `whisper_args` and
//! `detected` alone; `tests/fixtures/voice/bin/ffmpeg` is their stand-in and reads the same.

use crate::config::Config;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// The image's voice folder, root-owned and world-readable: `bin/ffmpeg`, `bin/whisper-cli`,
/// `models/`, `py/bin/python` with Piper, `voices/`. Absent in an image built with `VOICE=0`.
pub const DIR: &str = "/opt/zephyr/voice";
/// The uid and gid the decoders run as.
pub const NOBODY: u32 = 65534;
/// `voice.reply`'s values.
pub const REPLIES: [&str; 3] = ["off", "mirror", "always"];
/// whisper's language codes, the only values `voice.languages` takes.
pub const LANGUAGES: &[&str] = &[
    "ko", "en", "zh", "de", "es", "ru", "fr", "ja", "pt", "tr", "pl", "ca", "nl", "ar", "sv", "it",
    "id", "hi", "fi", "vi", "he", "uk", "el", "ms", "cs", "ro", "da", "hu", "ta", "no", "th", "ur",
    "hr", "bg", "lt", "la", "mi", "ml", "cy", "sk", "te", "fa", "lv", "bn", "sr", "az", "sl", "kn",
    "et", "mk", "br", "eu", "is", "hy", "ne", "mn", "bs", "kk", "sq", "sw", "gl", "mr", "pa", "si",
    "km", "sn", "yo", "so", "af", "oc", "ka", "be", "tg", "sd", "gu", "am", "yi", "lo", "uz", "fo",
    "ht", "ps", "tk", "nn", "mt", "sa", "lb", "my", "bo", "tl", "mg", "as", "tt", "haw", "ln",
    "ha", "ba", "jw", "su", "yue",
];

const FFMPEG_LIMIT: Duration = Duration::from_secs(30);
/// A transcript longer than this is cut.
const MAX_TEXT: usize = 4000;
/// What is kept of a process's output; one that writes more blocks and is killed at its limit.
const MAX_OUT: u64 = 64 * 1024 * 1024;
/// 16 kHz mono s16: the WAV ffmpeg writes for whisper, after its 44-byte header.
const WAV_HEADER: u64 = 44;
const WAV_BYTES_PER_SEC: u64 = 32000;
const MODEL: &str = "models/ggml-small-q5_1.bin";
const VAD_MODEL: &str = "models/ggml-silero-v5.1.2.bin";

/// What came of one voice note.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Heard {
    Said { lang: String, text: String },
    Silence,
    TooLong,
    Failed,
    TimedOut,
}

/// Why a process gave nothing.
enum Stop {
    Failed,
    TimedOut,
}

/// Whether this image has voice (built with `VOICE=1`).
pub fn present(cfg: &Config) -> bool {
    cfg.voice_dir.join("bin/whisper-cli").exists()
}

/// `5:00`.
pub fn m_ss(secs: u64) -> String {
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// The line that goes under a voice note's own (DESIGN §4.3). It starts with `[` and is one
/// line, so what was said is never the start of the row's text and never a line of its own.
pub fn line(heard: &Heard, max_secs: u64) -> String {
    match heard {
        Heard::Said { lang, text } => format!(
            "[transcript of the voice note above, by zephyr ({lang}), may be wrong: \"{}\"]",
            clean(text)
        ),
        Heard::Silence => "[voice note above: no speech heard]".into(),
        Heard::TooLong => format!(
            "[voice note above: not transcribed: longer than {}]",
            m_ss(max_secs)
        ),
        Heard::Failed => "[voice note above: transcription failed]".into(),
        Heard::TimedOut => "[voice note above: transcription timed out]".into(),
    }
}

/// One line: whitespace runs are one space, control characters and direction marks are gone,
/// cut at `MAX_TEXT` characters.
fn clean(text: &str) -> String {
    let mark = |c: char| matches!(c, '\u{200e}' | '\u{200f}' | '\u{061c}' | '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}');
    let words: Vec<String> = text
        .split_whitespace()
        .map(|w| w.chars().filter(|c| !c.is_control() && !mark(*c)).collect())
        .filter(|w: &String| !w.is_empty())
        .collect();
    words.join(" ").chars().take(MAX_TEXT).collect()
}

/// ffmpeg's argv to decode fd 3 into the WAV whisper reads, on fd 4, at most `secs` of it.
fn decode_args(secs: u64) -> Vec<String> {
    let secs = secs.to_string();
    [
        "-nostdin",
        "-loglevel",
        "error",
        "-y",
        "-i",
        "/proc/self/fd/3",
        "-t",
        &secs,
        "-vn",
        "-ar",
        "16000",
        "-ac",
        "1",
        "-f",
        "wav",
        "/proc/self/fd/4",
    ]
    .map(String::from)
    .to_vec()
}

/// whisper-cli's argv for the WAV on fd 3; `lang` is a code or `auto`. No `-np`: it would also
/// drop the detected-language line, and the text is all that goes to stdout either way.
fn whisper_args(dir: &Path, lang: &str, gpu: bool) -> Vec<String> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let threads = (cores / 2).clamp(1, 8).to_string();
    let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
    let (model, vad) = (path(MODEL), path(VAD_MODEL));
    let mut args = vec![
        "-m",
        &model,
        "--vad",
        "-vm",
        &vad,
        "-l",
        lang,
        "-nt",
        "-t",
        &threads,
        "-f",
        "/proc/self/fd/3",
    ];
    if !gpu {
        args.push("-ng");
    }
    args.into_iter().map(String::from).collect()
}

/// The language whisper-cli says it detected under `-l auto`, from its stderr:
/// `whisper_full_with_state: auto-detected language: ko (p = 0.97)`.
fn detected(stderr: &[u8]) -> Option<String> {
    const MARK: &str = "auto-detected language: ";
    let text = String::from_utf8_lossy(stderr);
    let rest = &text[text.find(MARK)? + MARK.len()..];
    let lang: String = (rest.chars())
        .take_while(|c| c.is_ascii_lowercase())
        .collect();
    LANGUAGES.contains(&lang.as_str()).then_some(lang)
}

/// Whether whisper is given the GPU: a device is there, and no run on it has failed since the
/// daemon started.
pub fn gpu() -> bool {
    use std::os::unix::fs::PermissionsExt;
    static FOUND: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let found = *FOUND.get_or_init(|| {
        // An NVIDIA device, or a render node `nobody` may open.
        let open = |e: std::fs::DirEntry| {
            e.file_name().to_string_lossy().starts_with("renderD")
                && e.metadata()
                    .is_ok_and(|m| m.permissions().mode() & 0o006 == 0o006)
        };
        Path::new("/dev/nvidia0").exists()
            || std::fs::read_dir("/dev/dri").is_ok_and(|d| d.flatten().any(open))
    });
    found && !GPU_FAILED.load(Ordering::Relaxed)
}

static GPU_FAILED: AtomicBool = AtomicBool::new(false);

/// An anonymous file in memory: what a decoder reads or writes instead of a path.
struct Mem(std::fs::File);

impl Mem {
    fn new(bytes: &[u8]) -> std::io::Result<Mem> {
        let flags = libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING;
        // Safety: a C string literal and plain flags; the result is checked.
        let fd = unsafe { libc::memfd_create(c"zephyr-voice".as_ptr(), flags) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Safety: `fd` is ours and open.
        let first = unsafe { std::fs::File::from_raw_fd(fd) };
        // Above the numbers a child is handed it under (`run`), so no `dup2` there lands on it.
        // Safety: a valid fd; the result is checked.
        let fd = unsafe { libc::fcntl(first.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 10) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Safety: as above.
        let mem = Mem(unsafe { std::fs::File::from_raw_fd(fd) });
        mem.0.write_all_at(bytes, 0)?;
        Ok(mem)
    }

    /// No write, growth or cut from here on, by anyone.
    fn seal(&self) -> std::io::Result<()> {
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // Safety: a valid fd; the result is checked.
        match unsafe { libc::fcntl(self.0.as_raw_fd(), libc::F_ADD_SEALS, seals) } {
            0 => Ok(()),
            _ => Err(std::io::Error::last_os_error()),
        }
    }

    fn sealed(bytes: &[u8]) -> std::io::Result<Mem> {
        let mem = Mem::new(bytes)?;
        mem.seal()?;
        Ok(mem)
    }

    fn len(&self) -> u64 {
        self.0.metadata().map_or(0, |m| m.len())
    }
}

struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

/// Runs `prog` as `nobody` (when the daemon is root: decision 2 of the lane's plan) with a
/// cleared environment, in `/`, in its own process group, at nice 10, each of `fds` at its
/// number, and kills the group at `limit`.
async fn run(
    prog: &Path,
    args: &[String],
    fds: &[(&Mem, i32)],
    stdin: Stdio,
    limit: Duration,
) -> Result<Out, Stop> {
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .env_clear()
        .current_dir("/")
        .stdin(stdin)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    // Safety: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        // std also drops the supplementary groups, and with the uid go the capabilities.
        cmd.uid(NOBODY).gid(NOBODY);
    }
    let dups: Vec<(i32, i32)> = fds.iter().map(|(m, n)| (m.0.as_raw_fd(), *n)).collect();
    // Safety: dup2 and nice are async-signal-safe and allocate nothing. `dup2` clears
    // close-on-exec on the copy; the sources are 10 or above (`Mem::new`).
    unsafe {
        cmd.pre_exec(move || {
            for (from, to) in &dups {
                if libc::dup2(*from, *to) < 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            libc::nice(10);
            Ok(())
        });
    }
    let mut child = cmd.spawn().map_err(|e| {
        eprintln!("zephyr: voice: starting {}: {e}", prog.display());
        Stop::Failed
    })?;
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (o, e) = (child.stdout.take(), child.stderr.take());
    let (mut o, mut e) = (
        o.expect("piped").take(MAX_OUT),
        e.expect("piped").take(MAX_OUT),
    );
    let all = async {
        tokio::join!(
            o.read_to_end(&mut out),
            e.read_to_end(&mut err),
            child.wait()
        )
        .2
    };
    let status = tokio::time::timeout(limit, all).await;
    let Ok(status) = status else {
        if let Some(pid) = child.id() {
            // Safety: the child is not reaped yet, so the pid, its group's id, is still its own.
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
        let _ = child.wait().await;
        return Err(Stop::TimedOut);
    };
    Ok(Out {
        ok: status.is_ok_and(|s| s.success()),
        stdout: out,
        stderr: err,
    })
}

/// Transcribes `audio`, bytes the caller fetched itself, and gives the audio's length in
/// seconds (0 when ffmpeg could not read it). One note at a time, so the browser and the agent
/// keep their cores and a channel's rows keep their order.
pub async fn transcribe(cfg: &Config, audio: &[u8]) -> (Heard, u64) {
    static ONE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
    let _one = ONE.acquire().await;
    let mut secs = 0;
    let heard = match hear(cfg, audio, &mut secs).await {
        Ok(heard) => heard,
        Err(Stop::Failed) => Heard::Failed,
        Err(Stop::TimedOut) => Heard::TimedOut,
    };
    (heard, secs)
}

fn mem<T>(made: std::io::Result<T>) -> Result<T, Stop> {
    made.map_err(|e| {
        eprintln!("zephyr: voice: memfd: {e}");
        Stop::Failed
    })
}

async fn hear(cfg: &Config, audio: &[u8], secs: &mut u64) -> Result<Heard, Stop> {
    let (voice, dir) = (cfg.voice(), &cfg.voice_dir);
    let (input, wav) = (mem(Mem::sealed(audio))?, mem(Mem::new(&[]))?);
    // One second over the limit shows the note is too long without decoding the rest of it.
    let args = decode_args(voice.max_secs + 1);
    let fds = [(&input, 3), (&wav, 4)];
    let ffmpeg = dir.join("bin/ffmpeg");
    if !run(&ffmpeg, &args, &fds, Stdio::null(), FFMPEG_LIMIT)
        .await?
        .ok
    {
        return Err(Stop::Failed);
    }
    *secs = wav.len().saturating_sub(WAV_HEADER) / WAV_BYTES_PER_SEC;
    if *secs > voice.max_secs {
        return Ok(Heard::TooLong);
    }
    mem(wav.seal())?;
    let first = voice.languages[0].clone();
    let (lang, out) = if voice.languages.len() == 1 {
        (first.clone(), whisper(cfg, &wav, &first).await?)
    } else {
        let out = whisper(cfg, &wav, "auto").await?;
        match detected(&out.stderr) {
            Some(lang) if voice.languages.contains(&lang) => (lang, out),
            // Short Korean is sometimes taken for Japanese: again, as the list's first.
            _ => (first.clone(), whisper(cfg, &wav, &first).await?),
        }
    };
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(match text.as_str() {
        "" | "[BLANK_AUDIO]" => Heard::Silence,
        _ => Heard::Said { lang, text },
    })
}

/// One whisper run of `wav` as `lang`. A run that fails on the GPU is the last one there: it is
/// done again on the CPU, as every later one is.
async fn whisper(cfg: &Config, wav: &Mem, lang: &str) -> Result<Out, Stop> {
    let prog = cfg.voice_dir.join("bin/whisper-cli");
    let mut tries = vec![false];
    if gpu() {
        tries.insert(0, true);
    }
    for gpu in tries {
        let args = whisper_args(&cfg.voice_dir, lang, gpu);
        let out = run(&prog, &args, &[(wav, 3)], Stdio::null(), cfg.voice_wait).await?;
        if out.ok {
            return Ok(out);
        }
        if gpu {
            GPU_FAILED.store(true, Ordering::Relaxed);
        }
    }
    Err(Stop::Failed)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::PathBuf;

    /// A config whose voice folder is a copy of the fakes (`tests/fixtures/voice`), with a log
    /// `nobody` can append to.
    pub fn fakes(name: &str, toml: &str) -> Config {
        use std::os::unix::fs::PermissionsExt;
        let home = crate::test_dir(name);
        let dir = home.join("voice");
        let from = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/voice");
        for file in ["bin/ffmpeg", "bin/whisper-cli", "py/bin/python"] {
            std::fs::create_dir_all(dir.join(file).parent().unwrap()).unwrap();
            std::fs::copy(from.join(file), dir.join(file)).unwrap();
        }
        std::fs::write(dir.join("log"), "").unwrap();
        std::fs::set_permissions(dir.join("log"), std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut cfg: Config =
            toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n{toml}")).unwrap();
        cfg.validate().unwrap();
        cfg.voice_dir = dir;
        cfg
    }

    /// The fakes' log: one object per run of `prog`.
    pub fn log(cfg: &Config, prog: &str) -> Vec<serde_json::Value> {
        let text = std::fs::read_to_string(cfg.voice_dir.join("log")).unwrap();
        (text.lines())
            .map(|l| serde_json::from_str::<serde_json::Value>(l).unwrap())
            .filter(|l| l["prog"] == prog)
            .collect()
    }

    pub fn sha256(bytes: &[u8]) -> String {
        data_encoding::HEXLOWER.encode(&crate::web::passkey::sha256(bytes))
    }

    fn mark(cfg: &Config, name: &str, text: &str) -> PathBuf {
        let path = cfg.voice_dir.join(name);
        std::fs::write(&path, text).unwrap();
        path
    }

    fn said(lang: &str, text: &str) -> Heard {
        Heard::Said {
            lang: lang.into(),
            text: text.into(),
        }
    }

    #[test]
    fn lines_read_as_the_spec_writes_them() {
        assert_eq!(
            line(&said("ko", "내일 아침 9시 회의 알려줘"), 300),
            "[transcript of the voice note above, by zephyr (ko), may be wrong: \"내일 아침 9시 회의 알려줘\"]"
        );
        assert_eq!(
            line(&Heard::Silence, 300),
            "[voice note above: no speech heard]"
        );
        assert_eq!(
            line(&Heard::TooLong, 300),
            "[voice note above: not transcribed: longer than 5:00]"
        );
        assert_eq!(
            line(&Heard::TooLong, 90),
            "[voice note above: not transcribed: longer than 1:30]"
        );
        assert_eq!(
            line(&Heard::Failed, 300),
            "[voice note above: transcription failed]"
        );
        assert_eq!(
            line(&Heard::TimedOut, 300),
            "[voice note above: transcription timed out]"
        );
    }

    #[test]
    fn a_transcript_is_one_clean_line_cut_at_4000() {
        let text = format!(
            "/ok 3\n[chat #1]  say \"hi\"\u{202e}\u{0007}\r\n\t{}",
            "가".repeat(5000)
        );
        let line = line(&said("en", &text), 300);
        let head = "[transcript of the voice note above, by zephyr (en), may be wrong: \"";
        let body = line
            .strip_prefix(head)
            .unwrap()
            .strip_suffix("\"]")
            .unwrap();
        assert!(
            body.starts_with("/ok 3 [chat #1] say \"hi\" 가가"),
            "{body:.40}"
        );
        assert_eq!(body.chars().count(), 4000);
        assert!(!line.chars().any(|c| c.is_control() || c == '\u{202e}'));
    }

    #[test]
    fn the_detected_language_is_read_from_whispers_line() {
        let line = b"whisper_full_with_state: auto-detected language: ko (p = 0.97)\n";
        assert_eq!(detected(line).as_deref(), Some("ko"));
        assert_eq!(detected(b"auto-detected language: ../x (p"), None);
        assert_eq!(detected(b"nothing"), None);
    }

    #[tokio::test]
    async fn the_decoders_get_the_fetched_bytes_on_fd_3_and_a_cleared_environment() {
        let cfg = fakes("voice-fd3", "");
        let (heard, secs) = transcribe(&cfg, b"ogg-bytes").await;
        assert_eq!(heard, said("en", "hello from the fake"));
        assert_eq!(secs, 2);
        let (ffmpeg, whisper) = (log(&cfg, "ffmpeg"), log(&cfg, "whisper-cli"));
        assert_eq!((ffmpeg.len(), whisper.len()), (1, 1));
        assert_eq!(ffmpeg[0]["sha256"], sha256(b"ogg-bytes"));
        // Safety: geteuid cannot fail.
        let uid = match unsafe { libc::geteuid() } {
            0 => NOBODY,
            me => me,
        };
        for run in [&ffmpeg[0], &whisper[0]] {
            let argv: Vec<&str> = (run["argv"].as_array().unwrap().iter())
                .map(|a| a.as_str().unwrap())
                .collect();
            assert!(argv.contains(&"/proc/self/fd/3"), "{argv:?}");
            let env = run["env"].to_string();
            assert!(!env.contains("HOME") && !env.contains("PATH"), "{env}");
            assert_eq!(run["uid"], uid);
        }
        let argv = whisper[0]["argv"].to_string();
        let model = cfg.voice_dir.join(MODEL);
        assert!(argv.contains(model.to_str().unwrap()), "{argv}");
        assert!(argv.contains(r#""-l","auto""#), "{argv}");
    }

    /// Review Focus 6: a decoder hung on a crafted file costs its limit, not the voice.
    #[tokio::test]
    async fn a_hung_whisper_is_killed_and_the_next_note_is_heard() {
        let mut cfg = fakes("voice-hang", "");
        cfg.voice_wait = Duration::from_millis(300);
        let hang = mark(&cfg, "hang", "");
        let (heard, _) = transcribe(&cfg, b"crafted").await;
        assert_eq!(
            line(&heard, 300),
            "[voice note above: transcription timed out]"
        );
        let pid = std::fs::read_to_string(cfg.voice_dir.join("hang.pid")).unwrap();
        // Safety: signal 0 only asks whether the process is there.
        let gone = unsafe { libc::kill(pid.trim().parse().unwrap(), 0) } != 0;
        assert!(gone, "the hung whisper was killed and reaped");
        std::fs::remove_file(hang).unwrap();
        cfg.voice_wait = Duration::from_secs(30);
        assert_eq!(
            transcribe(&cfg, b"next").await.0,
            said("en", "hello from the fake")
        );
    }

    #[tokio::test]
    async fn two_notes_are_transcribed_one_at_a_time() {
        let cfg = fakes("voice-one", "");
        mark(&cfg, "slow", "");
        tokio::join!(transcribe(&cfg, b"a"), transcribe(&cfg, b"b"));
        let mut runs: Vec<(f64, f64)> = (log(&cfg, "ffmpeg").iter())
            .chain(log(&cfg, "whisper-cli").iter())
            .map(|r| (r["start"].as_f64().unwrap(), r["end"].as_f64().unwrap()))
            .collect();
        assert_eq!(runs.len(), 4);
        runs.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // ffmpeg then whisper of one note, both before the other note's ffmpeg starts.
        assert!(runs.windows(2).all(|w| w[0].1 <= w[1].0), "{runs:?}");
    }

    #[tokio::test]
    async fn a_note_over_max_secs_is_not_given_to_whisper() {
        let cfg = fakes("voice-long", "[voice]\nmax_secs = 20");
        mark(&cfg, "secs", "400");
        let (heard, secs) = transcribe(&cfg, b"long").await;
        assert_eq!((heard, secs), (Heard::TooLong, 21));
        let ffmpeg = log(&cfg, "ffmpeg");
        assert!(ffmpeg[0]["argv"].to_string().contains(r#""-t","21""#));
        assert!(log(&cfg, "whisper-cli").is_empty());
    }

    #[tokio::test]
    async fn a_language_outside_the_list_is_run_again_as_the_first() {
        let cfg = fakes("voice-lang", "");
        mark(&cfg, "lang", "ja");
        mark(&cfg, "say", "안녕하세요");
        assert_eq!(transcribe(&cfg, b"a").await.0, said("ko", "안녕하세요"));
        let runs = log(&cfg, "whisper-cli");
        assert_eq!(runs.len(), 2);
        assert!(runs[0]["argv"].to_string().contains(r#""-l","auto""#));
        assert!(runs[1]["argv"].to_string().contains(r#""-l","ko""#));
        // With one language nothing is detected.
        let cfg = fakes("voice-lang-one", "[voice]\nlanguages = ['en']");
        mark(&cfg, "lang", "ja");
        assert!(matches!(transcribe(&cfg, b"a").await.0, Heard::Said { lang, .. } if lang == "en"));
        let runs = log(&cfg, "whisper-cli");
        assert_eq!(runs.len(), 1);
        assert!(runs[0]["argv"].to_string().contains(r#""-l","en""#));
    }

    #[tokio::test]
    async fn silence_and_a_broken_file_say_so() {
        let cfg = fakes("voice-silence", "");
        mark(&cfg, "say", "");
        assert_eq!(transcribe(&cfg, b"a").await.0, Heard::Silence);
        std::fs::remove_file(cfg.voice_dir.join("bin/ffmpeg")).unwrap();
        assert_eq!(transcribe(&cfg, b"a").await, (Heard::Failed, 0));
    }

    #[test]
    fn the_config_takes_only_whispers_codes() {
        let bad = |toml: &str| {
            let cfg: Config = toml::from_str(&format!("rpi = '/r'\nhome = '/h'\n{toml}")).unwrap();
            cfg.validate().unwrap_err().to_string()
        };
        assert!(bad("[voice]\nlanguages = ['-ng']").contains("language codes"));
        assert!(bad("[voice]\nlanguages = []").contains("language codes"));
        assert!(bad("[voice]\nreply = 'loud'").contains("off, mirror or always"));
        assert!(bad("[voice]\nmax_secs = 5").contains("10 to 900"));
        let cfg: Config = toml::from_str("rpi = '/r'\nhome = '/h'").unwrap();
        let v = cfg.voice();
        assert_eq!(
            (v.transcribe, v.reply.as_str(), v.max_secs),
            (true, "mirror", 300)
        );
        assert_eq!(v.languages, ["ko", "en"]);
    }
}
