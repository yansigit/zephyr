//! Local voice (DESIGN §4.3): an owner's voice note is transcribed by whisper.cpp. ffmpeg and
//! whisper-cli are parsers fed by files people send, so they run from the image's root-owned
//! [`DIR`] as `nobody`: not root, whose files an exploit would reach, and not the agent, which
//! could ptrace them and rewrite the text. They read sealed memfds, never a path the agent can
//! write, and the floor gives uid 65534 no network (`egress::ruleset`).
//!
//! A reply can also be spoken: Piper makes the speech and ffmpeg encodes it, the same way.
//!
//! The real binaries' argv and output shapes are in `decode_args`, `whisper_args`, `detected`,
//! `piper_args` and `encode_args` alone; `tests/fixtures/voice/bin/ffmpeg` is their stand-in
//! and reads the same.

use crate::config::Config;
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::FileExt;
use std::path::Path;
use std::process::Stdio;
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
/// Piper's limit, and its encode's.
const SPEAK_LIMIT: Duration = Duration::from_secs(60);
/// A reply longer than this, once marks and URLs are gone, is not spoken: about two minutes.
const MAX_SPOKEN: usize = 1500;
/// The channels that can carry a voice note back.
const SPOKEN_ON: [&str; 4] = ["telegram", "signal", "discord", "imessage"];
const VOICE_EN: &str = "voices/en_US-lessac-medium.onnx";
/// In the image only with `KO_VOICE=1` (CC BY-NC-SA 4.0); without it Korean replies stay text.
const VOICE_KO: &str = "voices/ko_KR-kss-medium.onnx";
/// What Piper's `medium` voices write with `--output-raw`: s16le mono at this rate.
const PIPER_RATE: &str = "22050";
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
    Said {
        lang: String,
        text: String,
    },
    Silence,
    TooLong,
    Failed,
    TimedOut,
    /// Nothing ran: there is no network floor (`floored`).
    NoFloor,
}

/// Why a process gave nothing.
enum Stop {
    Failed,
    TimedOut,
}

/// Whether the voice programs may run at all: only under the network floor (DESIGN §4.20),
/// whose rules are what keep `nobody` off the network. Under `network = "host"` a decoder fed a
/// crafted file would be on the host's network, so nothing is decoded, transcribed or spoken.
pub fn floored(cfg: &Config) -> bool {
    crate::egress::bridge() || cfg.voice_off_floor
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
        Heard::NoFloor => "[transcript skipped: needs the network floor]".into(),
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

/// ffmpeg's argv to decode fd 3 into the WAV whisper reads, on stdout, at most `secs` of it (as
/// measured with the image's static ffmpeg 7.1).
fn decode_args(secs: u64) -> Vec<String> {
    let secs = secs.to_string();
    let args = "-hide_banner -nostdin -loglevel error -i /proc/self/fd/3 -vn -ar 16000 -ac 1 \
                -c:a pcm_s16le -f wav -t";
    (args.split(' ').chain([secs.as_str(), "pipe:1"]))
        .map(String::from)
        .collect()
}

/// whisper-cli's argv for the WAV on fd 3 (`-f -` does not work); `lang` is a code or `auto`.
/// As measured with 1.9.5: no `-np`, which would also drop the detected-language line, and with
/// `-nt` the text is all that goes to stdout either way (nothing at all when the VAD finds no
/// speech).
fn whisper_args(dir: &Path, lang: &str) -> Vec<String> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let threads = (cores / 2).clamp(1, 8).to_string();
    let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
    let (model, vad) = (path(MODEL), path(VAD_MODEL));
    // shortcut: CPU only; the GPU needs a shader cache folder for nobody
    // (__GL_SHADER_DISK_CACHE_PATH), then drop -ng
    [
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
        "-ng",
        "-f",
        "/proc/self/fd/3",
    ]
    .map(String::from)
    .to_vec()
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

/// An anonymous file in memory: what a decoder reads or writes instead of a path.
struct Mem(std::fs::File);

impl Mem {
    /// Holding `bytes`, with no write, growth or cut from here on, by anyone.
    fn sealed(bytes: &[u8]) -> std::io::Result<Mem> {
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
        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // Safety: a valid fd; the result is checked.
        match unsafe { libc::fcntl(mem.0.as_raw_fd(), libc::F_ADD_SEALS, seals) } {
            0 => Ok(mem),
            _ => Err(std::io::Error::last_os_error()),
        }
    }
}

struct Out {
    ok: bool,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
}

fn root() -> bool {
    // Safety: geteuid cannot fail.
    unsafe { libc::geteuid() == 0 }
}

/// The one variable a decoder gets when the daemon is not root, and so cannot drop to
/// `nobody`: this process's own mark, by which `sweep` finds what its runs left.
fn mark() -> (&'static str, &'static str) {
    static MARK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    let value = MARK.get_or_init(|| format!("{}-{}", std::process::id(), crate::unix_millis()));
    ("ZEPHYR_VOICE_RUN", value)
}

/// Kills whatever the voice programs left running. A process group does not hold a decoder
/// that calls `setsid`, so this goes by what it cannot shed: as root, every process whose real
/// uid is 65534, which runs nothing but these programs, one run at a time (`Turn`). When the
/// daemon is not root nothing was dropped to `nobody`, and it is every process of the daemon's
/// own uid that carries this process's `mark` in its environment: enough for a test, which
/// must never kill a stranger's process, and no boundary, since a process can rewrite its own.
fn sweep() {
    let (name, value) = mark();
    let mark = format!("{name}={value}");
    // Safety: getuid cannot fail.
    let me = unsafe { libc::getuid() };
    let marked = |pid: i32| {
        std::fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|env| env.split(|b| *b == 0).any(|v| v == mark.as_bytes()))
    };
    // By uid only in the floored container, whose pid namespace holds nothing else of nobody's:
    // never a root daemon on a host, where uid 65534 runs other people's services.
    let left = |pid: i32, uid: u32| match root() && crate::egress::bridge() {
        true => uid == NOBODY,
        false => uid == me && pid != std::process::id() as i32 && marked(pid),
    };
    // Again while something was found: a process may fork as it is killed.
    for _ in 0..3 {
        if crate::procs::kill_all(left) == 0 {
            break;
        }
    }
}

/// The one turn the voice programs run in, transcription and speech alike: `sweep` kills
/// every process of `nobody`, so no other run of theirs may be going. When the turn ends, also
/// by its future being dropped mid-run (the adapter's task aborted, a timeout around it), what
/// it left is swept before the next one starts.
struct Turn(#[allow(dead_code)] tokio::sync::SemaphorePermit<'static>);

impl Turn {
    async fn take() -> Turn {
        static ONE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        Turn(ONE.acquire().await.expect("never closed"))
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        sweep();
    }
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
    if root() {
        // std also drops the supplementary groups, and with the uid go the capabilities.
        cmd.uid(NOBODY).gid(NOBODY);
    } else {
        // No uid to tell the run's processes by (a test, a daemon that is not root): the
        // sweep finds them by this instead.
        let (name, value) = mark();
        cmd.env(name, value);
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
    let pid = child.id().expect("not waited on yet") as i32;
    // Safety: the group is the child's own (`process_group(0)`), and each call below is made
    // while the child is not reaped, so the pid cannot have gone to another process.
    let kill = move || unsafe { libc::killpg(pid, libc::SIGKILL) };
    // The child's end, seen without reaping it.
    let exited = tokio::task::spawn_blocking(move || loop {
        // Safety: `info` is plain data the call fills in; the pid is our child's.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOWAIT;
        let r = unsafe { libc::waitid(libc::P_PID, pid as libc::id_t, &mut info, flags) };
        if r == 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted {
            break;
        }
    });
    let (mut out, mut err) = (Vec::new(), Vec::new());
    let (o, e) = (child.stdout.take(), child.stderr.take());
    let (mut o, mut e) = (
        o.expect("piped").take(MAX_OUT),
        e.expect("piped").take(MAX_OUT),
    );
    let all = async {
        tokio::join!(o.read_to_end(&mut out), e.read_to_end(&mut err), async {
            let _ = exited.await;
            // Whatever it forked goes with it: nothing is left running as `nobody`, and a
            // child holding the pipes open does not keep the reads waiting.
            kill();
        })
    };
    let timed_out = tokio::time::timeout(limit, all).await.is_err();
    // At the limit this is what stops it; otherwise the group is already gone.
    kill();
    let status = child.wait().await;
    if timed_out {
        return Err(Stop::TimedOut);
    }
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
    if !floored(cfg) {
        return (Heard::NoFloor, 0);
    }
    let _turn = Turn::take().await;
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
    let input = mem(Mem::sealed(audio))?;
    // One second over the limit shows the note is too long without decoding the rest of it.
    let args = decode_args(voice.max_secs + 1);
    let ffmpeg = dir.join("bin/ffmpeg");
    let decoded = run(&ffmpeg, &args, &[(&input, 3)], Stdio::null(), FFMPEG_LIMIT).await?;
    if !decoded.ok {
        return Err(Stop::Failed);
    }
    *secs = (decoded.stdout.len() as u64).saturating_sub(WAV_HEADER) / WAV_BYTES_PER_SEC;
    if *secs > voice.max_secs {
        return Ok(Heard::TooLong);
    }
    let wav = mem(Mem::sealed(&decoded.stdout))?;
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

/// One whisper run of `wav` as `lang`.
async fn whisper(cfg: &Config, wav: &Mem, lang: &str) -> Result<Out, Stop> {
    let prog = cfg.voice_dir.join("bin/whisper-cli");
    let args = whisper_args(&cfg.voice_dir, lang);
    let out = run(&prog, &args, &[(wav, 3)], Stdio::null(), cfg.voice_wait).await?;
    match out.ok {
        true => Ok(out),
        false => Err(Stop::Failed),
    }
}

/// Whether a reply to a row on `channel` is also spoken (`voice.reply`): under `mirror` one to
/// a voice note (`derived`), under `always` every one, where a voice note can be sent and the
/// image has Piper.
pub fn replies_aloud(cfg: &Config, channel: &str, derived: Option<&str>) -> bool {
    let on = match cfg.voice().reply.as_str() {
        "always" => true,
        "mirror" => derived == Some("voice"),
        _ => false,
    };
    on && SPOKEN_ON.contains(&channel)
        && floored(cfg)
        && cfg.voice_dir.join("py/bin/python").exists()
}

/// `text` as it is spoken: markdown marks and URLs gone. `None` for what is read, never heard:
/// a notice of the daemon's, a reply with a code block, one over `MAX_SPOKEN` characters, or
/// nothing left to say.
pub fn speakable(text: &str) -> Option<String> {
    if text.trim_start().starts_with("[zephyr]") || text.contains("```") {
        return None;
    }
    let line = |line: &str| -> String {
        // A link's address becomes a word of its own, and goes with the other URLs.
        let line = line.replace("](", "] (");
        let words: Vec<String> = line
            .split_whitespace()
            .filter(|w| !w.contains("://"))
            .map(|w| w.chars().filter(|c| !"*_`#~>|[]".contains(*c)).collect())
            // A list's dash or a rule: nothing to say.
            .filter(|w: &String| w.chars().any(char::is_alphanumeric))
            .collect();
        words.join(" ")
    };
    let lines: Vec<String> = (text.lines().map(line)).filter(|l| !l.is_empty()).collect();
    let said = lines.join("\n");
    (!said.is_empty() && said.chars().count() <= MAX_SPOKEN).then_some(said)
}

/// The file name a channel's voice note has, and its codec: Ogg Opus where the platform shows
/// that as a voice message, AAC in M4A elsewhere.
pub fn format_for(channel: &str) -> (&'static str, &'static str) {
    match channel {
        "telegram" | "discord" => ("voice.ogg", "opus"),
        _ => ("voice.m4a", "aac"),
    }
}

/// The Piper voice for `text`: Korean when Hangul is at least 30% of its letters, else English.
/// `None` when the image does not have it.
fn voice_for(dir: &Path, text: &str) -> Option<std::path::PathBuf> {
    let hangul = |c: &char| matches!(c, '\u{ac00}'..='\u{d7a3}' | '\u{1100}'..='\u{11ff}' | '\u{3130}'..='\u{318f}');
    let letters = text.chars().filter(|c| c.is_alphabetic()).count();
    let korean = text.chars().filter(hangul).count() * 10 >= letters * 3 && letters > 0;
    let voice = dir.join(if korean { VOICE_KO } else { VOICE_EN });
    voice.exists().then_some(voice)
}

/// Piper's argv: the text comes on stdin, raw PCM goes to stdout.
fn piper_args(voice: &Path) -> Vec<String> {
    let voice = voice.to_string_lossy();
    ["-m", "piper", "-m", &voice, "--output-raw"]
        .map(String::from)
        .to_vec()
}

/// ffmpeg's argv to encode Piper's PCM on fd 3 to stdout, as measured with the image's static
/// ffmpeg 7.1. M4A to a pipe has to be fragmented: the muxer cannot seek back.
fn encode_args(codec: &str) -> Vec<String> {
    let out: &[&str] = match codec {
        "opus" => &["-c:a", "libopus", "-b:a", "32k", "-f", "ogg"],
        _ => &[
            "-c:a",
            "aac",
            "-movflags",
            "frag_keyframe+empty_moov",
            "-f",
            "ipod",
        ],
    };
    // The rate before `-i` is the input's: raw PCM carries none of its own.
    ([
        "-hide_banner",
        "-nostdin",
        "-loglevel",
        "error",
        "-f",
        "s16le",
    ]
    .into_iter())
    .chain(["-ar", PIPER_RATE, "-ac", "1", "-i", "/proc/self/fd/3"])
    .chain(out.iter().copied())
    .chain(["pipe:1"])
    .map(String::from)
    .collect()
}

/// `text` spoken, as the file `channel` takes (`format_for`), or `None`: not speakable, no
/// voice for its language in this image, or Piper or ffmpeg failed. Both run as the decoders do.
pub async fn speak(cfg: &Config, text: &str, channel: &str) -> Option<Vec<u8>> {
    if !floored(cfg) {
        return None;
    }
    let text = speakable(text)?;
    let dir = &cfg.voice_dir;
    let voice = voice_for(dir, &text)?;
    let _turn = Turn::take().await;
    let made = async {
        let said = mem(Mem::sealed(text.as_bytes()))?;
        let stdin = Stdio::from(mem(said.0.try_clone())?);
        let python = dir.join("py/bin/python");
        let piper = run(&python, &piper_args(&voice), &[], stdin, SPEAK_LIMIT).await?;
        // Piper's stderr is never empty (onnxruntime warns): the exit code and the PCM count.
        if !piper.ok || piper.stdout.is_empty() {
            return Err(Stop::Failed);
        }
        let pcm = mem(Mem::sealed(&piper.stdout))?;
        let args = encode_args(format_for(channel).1);
        let ffmpeg = dir.join("bin/ffmpeg");
        let out = run(&ffmpeg, &args, &[(&pcm, 3)], Stdio::null(), SPEAK_LIMIT).await?;
        match out.ok {
            true => Ok(out.stdout),
            false => Err(Stop::Failed),
        }
    };
    match made.await {
        Ok(audio) if !audio.is_empty() => Some(audio),
        Ok(_) | Err(Stop::Failed) => {
            eprintln!("zephyr: voice: a reply could not be spoken");
            None
        }
        Err(Stop::TimedOut) => {
            eprintln!("zephyr: voice: speaking a reply timed out");
            None
        }
    }
}

/// The uid a decoder gets: `/usr/bin/id -u`, run as they are. For `doctor` and the in-image
/// test.
pub async fn uid() -> Option<u32> {
    let id = Path::new("/usr/bin/id");
    let _turn = Turn::take().await;
    let out = run(id, &["-u".into()], &[], Stdio::null(), FFMPEG_LIMIT).await;
    String::from_utf8_lossy(&out.ok()?.stdout)
        .trim()
        .parse()
        .ok()
}

/// `status`'s line, and `doctor`'s first.
pub fn status(cfg: &Config) -> String {
    if !present(cfg) {
        return "voice: not in this image (built with VOICE=0)".into();
    }
    if !floored(cfg) {
        return "voice: off without the network floor (host.toml network = \"bridge\")".into();
    }
    "voice: whisper small on CPU".into()
}

/// `doctor`'s lines, each `ok|warn <what>`: the models and voices that are there, the uid the
/// decoders get, and a one-second silent sample through ffmpeg and whisper within 30 s.
pub async fn doctor(cfg: &Config) -> Vec<String> {
    if present(cfg) && !floored(cfg) {
        return vec![format!("warn {}", status(cfg))];
    }
    let mut lines = vec![format!("ok   {}", status(cfg))];
    if !present(cfg) {
        return lines;
    }
    let there = |files: &[&str]| -> (Vec<String>, Vec<String>) {
        (files.iter().map(|f| f.to_string())).partition(|f| cfg.voice_dir.join(f).exists())
    };
    let (have, missing) = there(&["bin/ffmpeg", MODEL, VAD_MODEL, VOICE_EN]);
    match missing.is_empty() {
        true => lines.push(format!("ok   voice: {} are there", have.join(", "))),
        false => lines.push(format!("warn voice: {} missing", missing.join(", "))),
    }
    if !cfg.voice_dir.join(VOICE_KO).exists() {
        let note = "ok   voice: no Korean voice (built with KO_VOICE=0): Korean replies are text";
        lines.push(note.into());
    }
    // Safety: geteuid cannot fail.
    if unsafe { libc::geteuid() } == 0 {
        lines.push(match uid().await {
            Some(NOBODY) => "ok   voice: the decoders run as uid 65534".into(),
            other => format!("warn voice: the decoders run as {other:?}, not uid 65534"),
        });
    }
    // One second of silence, as the WAV whisper itself reads.
    let mut wav = Vec::from(*b"RIFF");
    let data = WAV_BYTES_PER_SEC as u32;
    wav.extend((36 + data).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    for n in [16u32, 0x0001_0001, 16000, 32000, 0x0010_0002] {
        wav.extend(n.to_le_bytes());
    }
    wav.extend(b"data");
    wav.extend(data.to_le_bytes());
    wav.resize(wav.len() + data as usize, 0);
    let sample = tokio::time::timeout(Duration::from_secs(30), transcribe(cfg, &wav)).await;
    lines.push(match sample {
        Ok((Heard::Silence | Heard::Said { .. }, _)) => {
            "ok   voice: a silent sample is transcribed".into()
        }
        Ok((heard, _)) => format!("warn voice: a silent sample gave {heard:?}"),
        Err(_) => "warn voice: a silent sample took over 30 s".into(),
    });
    lines
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
            let fake = std::fs::read_to_string(from.join(file)).unwrap();
            crate::test_exe(&dir.join(file), &fake);
        }
        std::fs::create_dir_all(dir.join("voices")).unwrap();
        std::fs::write(dir.join(VOICE_EN), "").unwrap();
        std::fs::write(dir.join("log"), "").unwrap();
        std::fs::set_permissions(dir.join("log"), std::fs::Permissions::from_mode(0o666)).unwrap();
        let mut cfg: Config =
            toml::from_str(&format!("rpi = '/r'\nhome = {home:?}\n{toml}")).unwrap();
        cfg.validate().unwrap();
        cfg.voice_dir = dir;
        // The test process is not a container under the network floor, which voice needs.
        cfg.voice_off_floor = true;
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
        assert!(
            argv.contains(r#""-ng""#) && !argv.contains(r#""-np""#),
            "{argv}"
        );
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
        // Killed and reaped wherever the limit found it, asleep in its hang or (on a loaded
        // machine) still starting: no child of this thread is left, running or a zombie.
        let left = std::fs::read_to_string("/proc/thread-self/children").unwrap();
        assert_eq!(left, "", "the hung whisper was not killed and reaped");
        std::fs::remove_file(hang).unwrap();
        cfg.voice_wait = Duration::from_secs(30);
        assert_eq!(
            transcribe(&cfg, b"next").await.0,
            said("en", "hello from the fake")
        );
    }

    /// A decoder that forks and exits leaves nothing behind running as `nobody`.
    #[tokio::test]
    async fn a_decoders_child_does_not_outlive_it() {
        let cfg = fakes("voice-fork", "");
        mark(&cfg, "fork", "");
        assert_eq!(
            transcribe(&cfg, b"crafted").await.0,
            said("en", "hello from the fake")
        );
        gone(&cfg, "fork.pid").await;
    }

    /// Waits until the process whose pid the fake wrote to `file` is gone: killed, though it
    /// may be a zombie its new parent has yet to reap.
    async fn gone(cfg: &Config, file: &str) {
        let pid = std::fs::read_to_string(cfg.voice_dir.join(file)).unwrap();
        let stat = || std::fs::read_to_string(format!("/proc/{}/stat", pid.trim()));
        let gone = || stat().map_or(true, |s| s.contains(") Z ") || s.contains(") X "));
        for _ in 0..200 {
            if gone() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{file}: the process is still there: {:?}", stat());
    }

    /// A session of its own takes a child out of the decoder's process group: it is swept all
    /// the same when the run ends.
    #[tokio::test]
    async fn a_child_in_a_session_of_its_own_does_not_outlive_the_decoder() {
        let cfg = fakes("voice-setsid", "");
        mark(&cfg, "setsid", "");
        assert_eq!(
            transcribe(&cfg, b"crafted").await.0,
            said("en", "hello from the fake")
        );
        gone(&cfg, "setsid.pid").await;
    }

    /// The adapter's task aborted mid-run, or a timeout around it: the decoder and what it
    /// left still go.
    #[tokio::test]
    async fn a_dropped_transcription_leaves_nothing_running() {
        let cfg = fakes("voice-dropped", "");
        mark(&cfg, "setsid", "");
        mark(&cfg, "hang", "");
        let theirs = cfg.clone();
        let run = tokio::spawn(async move { transcribe(&theirs, b"crafted").await });
        let (hung, left) = (
            cfg.voice_dir.join("hang.pid"),
            cfg.voice_dir.join("setsid.pid"),
        );
        // Until both fakes are asleep, however long a loaded machine (and the voice's one turn,
        // which other tests' notes may hold) takes: the run's own limit ends it if they never are.
        let written = |p: &Path| std::fs::read_to_string(p).is_ok_and(|s| !s.is_empty());
        while !(written(&hung) && written(&left)) {
            assert!(
                !run.is_finished(),
                "the run ended before its fakes were asleep"
            );
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());
        gone(&cfg, "hang.pid").await;
        gone(&cfg, "setsid.pid").await;
        // And the turn is free again.
        std::fs::remove_file(cfg.voice_dir.join("hang")).unwrap();
        std::fs::remove_file(cfg.voice_dir.join("setsid")).unwrap();
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
        assert!(ffmpeg[0]["argv"]
            .to_string()
            .contains(r#""-t","21","pipe:1""#));
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
    fn notices_code_and_long_replies_are_not_spoken() {
        assert_eq!(speakable("[zephyr] paused"), None);
        assert_eq!(speakable("run this:\n```sh\nls\n```"), None);
        assert_eq!(speakable(&"word ".repeat(301)), None, "1504 characters");
        assert!(speakable(&"word ".repeat(300)).is_some(), "1499");
        assert_eq!(speakable("https://example.com/a"), None, "nothing left");
        assert_eq!(
            speakable("**Done.** See [the docs](https://example.com/a) or https://x.test\n\n- `one`\n- two"),
            Some("Done. See the docs or\none\ntwo".into())
        );
    }

    #[test]
    fn hangul_picks_the_korean_voice_and_a_missing_one_speaks_nothing() {
        let cfg = fakes("voice-pick", "");
        let dir = &cfg.voice_dir;
        assert_eq!(voice_for(dir, "hello there"), Some(dir.join(VOICE_EN)));
        // Built with KO_VOICE=0: a Korean reply stays text.
        assert_eq!(voice_for(dir, "내일 아침 9시에 알려 드릴게요"), None);
        std::fs::write(dir.join(VOICE_KO), "").unwrap();
        let ko = Some(dir.join(VOICE_KO));
        assert_eq!(voice_for(dir, "내일 아침 9시에 알려 드릴게요"), ko);
        assert_eq!(voice_for(dir, "네, OK"), ko, "1 of 3 letters");
        assert_eq!(
            voice_for(dir, "the word 네 means yes"),
            Some(dir.join(VOICE_EN))
        );
        std::fs::remove_file(dir.join(VOICE_EN)).unwrap();
        assert_eq!(voice_for(dir, "hello"), None);
    }

    #[tokio::test]
    async fn discord_gets_ogg_and_imessage_m4a() {
        assert_eq!(format_for("discord"), ("voice.ogg", "opus"));
        assert_eq!(format_for("telegram"), ("voice.ogg", "opus"));
        assert_eq!(format_for("imessage"), ("voice.m4a", "aac"));
        assert_eq!(format_for("signal"), ("voice.m4a", "aac"));
        let cfg = fakes("voice-speak", "");
        let ogg = speak(&cfg, "**hello** there", "discord").await.unwrap();
        assert_eq!(ogg, b"OggSpcm:hello there");
        let m4a = speak(&cfg, "hello there", "imessage").await.unwrap();
        assert_eq!(m4a, b"ftyppcm:hello there");
        let piper = &log(&cfg, "python")[0];
        assert_eq!(piper["sha256"], sha256(b"hello there"));
        let argv = piper["argv"].to_string();
        assert!(argv.contains("en_US-lessac-medium.onnx") && argv.contains("--output-raw"));
        assert!(!piper["env"].to_string().contains("HOME"));
        let encodes = log(&cfg, "ffmpeg");
        assert!(encodes[0]["argv"].to_string().contains("libopus"));
        assert!(encodes[1]["argv"].to_string().contains("ipod"));
        mark(&cfg, "piper_fail", "");
        assert_eq!(speak(&cfg, "hello", "discord").await, None);
        assert_eq!(speak(&cfg, "[zephyr] paused", "discord").await, None);
    }

    #[test]
    fn a_reply_is_spoken_only_where_the_setting_and_the_channel_allow() {
        let cfg = fakes("voice-aloud", "");
        assert!(replies_aloud(&cfg, "discord", Some("voice")));
        assert!(!replies_aloud(&cfg, "discord", None), "mirror: a typed row");
        assert!(!replies_aloud(&cfg, "chat", Some("voice")));
        let always = fakes("voice-aloud-always", "[voice]\nreply = 'always'");
        assert!(replies_aloud(&always, "imessage", None));
        assert!(!replies_aloud(&always, "web", None));
        let off = fakes("voice-aloud-off", "[voice]\nreply = 'off'");
        assert!(!replies_aloud(&off, "discord", Some("voice")));
        let mut none = cfg.clone();
        none.voice_dir = cfg.home.join("no-voice");
        assert!(!replies_aloud(&none, "discord", Some("voice")), "VOICE=0");
    }

    #[tokio::test]
    async fn doctor_names_what_is_there_and_hears_a_silent_sample() {
        let cfg = fakes("voice-doctor", "");
        mark(&cfg, "say", "");
        let lines = doctor(&cfg).await;
        assert!(
            lines[0].starts_with("ok   voice: whisper small on "),
            "{lines:?}"
        );
        // The fakes have no models.
        assert!(
            lines[1].starts_with("warn voice: models/ggml-small-q5_1.bin, "),
            "{lines:?}"
        );
        assert_eq!(
            lines.last().unwrap(),
            "ok   voice: a silent sample is transcribed"
        );
        let mut none = cfg.clone();
        none.voice_dir = cfg.home.join("no-voice");
        assert_eq!(
            doctor(&none).await,
            ["ok   voice: not in this image (built with VOICE=0)"]
        );
    }

    #[test]
    fn pipers_rate_is_given_for_the_input_not_the_output() {
        // After `-i` it would be the output's rate, the raw PCM would be read at ffmpeg's default
        // 44100 Hz, and the reply would be spoken at twice its speed.
        for codec in ["opus", "aac"] {
            let args = encode_args(codec);
            let at = |a: &str| args.iter().position(|x| x == a).unwrap();
            assert!(at("-ar") < at("-i"), "{args:?}");
            assert_eq!(args[at("-ar") + 1], PIPER_RATE);
        }
    }

    /// The rule that gives `nobody` no network exists only under the floor: without it a
    /// decoder would be on the host's network, so nothing is decoded or spoken.
    #[tokio::test]
    async fn without_the_floor_nothing_is_decoded_or_spoken() {
        let mut cfg = fakes("voice-no-floor", "[voice]\nreply = 'always'");
        cfg.voice_off_floor = false;
        let (heard, secs) = transcribe(&cfg, b"ogg-bytes").await;
        assert_eq!((&heard, secs), (&Heard::NoFloor, 0));
        assert_eq!(
            line(&heard, 300),
            "[transcript skipped: needs the network floor]"
        );
        assert_eq!(speak(&cfg, "hello there", "discord").await, None);
        assert!(!replies_aloud(&cfg, "discord", Some("voice")));
        let off = "voice: off without the network floor (host.toml network = \"bridge\")";
        assert_eq!(status(&cfg), off);
        assert_eq!(doctor(&cfg).await, [format!("warn {off}")]);
        let log = std::fs::read_to_string(cfg.voice_dir.join("log")).unwrap();
        assert_eq!(log, "", "no ffmpeg, no whisper, no Piper");
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
