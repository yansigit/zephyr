//! Local voice (DESIGN §4.3): an owner's voice note is transcribed by whisper.cpp. ffmpeg and
//! whisper-cli are parsers fed by files people send, so they run from the image's root-owned
//! [`DIR`] as `nobody`: not root, whose files an exploit would reach, and not the agent, which
//! could ptrace them and rewrite the text. They read sealed memfds, never a path the agent can
//! write, and the floor gives uid 65534 no network (`egress::ruleset`).
//!
//! A reply can also be spoken: Piper makes the speech and ffmpeg encodes it, the same way.
//!
//! A phone call with `calls.speech = "local"` keeps one whisper-server and one Piper loaded for
//! its length (`Call`, DESIGN §4.26), as `CALL_UID`, which the floor also gives no network.
//!
//! The real binaries' argv and output shapes are in `decode_args`, `whisper_args`, `detected`,
//! `piper_args`, `encode_args`, `server_args`, `PIPER_LINES` and `ask_ear` (the request to the
//! server and the reading of its reply) alone; `tests/fixtures/voice/bin/ffmpeg` is their
//! stand-in and reads the same.

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
/// The uid and gid a phone call's whisper-server and Piper run as (`zephyr-call` in the image,
/// no shell, no home; DESIGN §4.26): not `nobody`, so a call and a voice note never wait on
/// each other, and neither's sweep can take the other's processes.
pub const CALL_UID: u32 = 65533;
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
/// The smaller model a phone call's utterances are transcribed with, when the image has it
/// (`CALL_STT=1`): a turn of a call cannot wait for the small one. Measured on this project's
/// machine for a 3 s sentence, a process a run: small 3.8 s, base 1.1 s, with the same words
/// in English and Korean (DESIGN §4.26).
const CALL_MODEL: &str = "models/ggml-base-q5_1.bin";
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

/// Whether the image has the Piper voice a call in `lang` is spoken with (`en`, `ko`).
pub fn call_voice(cfg: &Config, lang: &str) -> bool {
    let voice = match lang {
        "en" => VOICE_EN,
        "ko" => VOICE_KO,
        _ => return false,
    };
    cfg.voice_dir.join(voice).exists()
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
fn whisper_args(dir: &Path, model: &str, lang: &str) -> Vec<String> {
    let cores = std::thread::available_parallelism().map_or(2, |n| n.get());
    let threads = (cores / 2).clamp(1, 8).to_string();
    let path = |p: &str| dir.join(p).to_string_lossy().into_owned();
    let (model, vad) = (path(model), path(VAD_MODEL));
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

/// Whose a run of the voice programs is: a voice note's or a spoken reply's (`nobody`, one at a
/// time), or a phone call's (`CALL_UID`, one call at a time). Each has its uid, its `Turn` and
/// its sweep.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Who {
    Note,
    Call,
}

impl Who {
    /// The uid and gid its programs run as when the daemon is root.
    fn uid(self) -> u32 {
        match self {
            Who::Note => NOBODY,
            Who::Call => CALL_UID,
        }
    }

    /// The uid its programs do run as: `uid`, or the daemon's own when it is not root.
    fn runs_as(self) -> u32 {
        // Safety: geteuid cannot fail.
        match root() {
            true => self.uid(),
            false => unsafe { libc::geteuid() },
        }
    }

    /// The one variable its programs get when the daemon is not root, and so cannot drop to
    /// their uid: this process's own mark, by which `sweep` finds what its runs left.
    fn mark(self) -> (&'static str, &'static str) {
        static MARK: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        let value = MARK.get_or_init(|| format!("{}-{}", std::process::id(), crate::unix_millis()));
        match self {
            Who::Note => ("ZEPHYR_VOICE_RUN", value),
            Who::Call => ("ZEPHYR_VOICE_CALL", value),
        }
    }
}

/// Kills whatever `who`'s programs left running. A process group does not hold a decoder that
/// calls `setsid`, so this goes by what it cannot shed: as root, every process whose real uid
/// is `who`'s, which runs nothing but these programs, one run (or call) at a time (`Turn`).
/// When the daemon is not root nothing was dropped to that uid, and it is every process of the
/// daemon's own uid that carries `who`'s `mark` in its environment: enough for a test, which
/// must never kill a stranger's process, and no boundary, since a process can rewrite its own.
fn sweep(who: Who) {
    let (name, value) = who.mark();
    let mark = format!("{name}={value}");
    // Safety: getuid cannot fail.
    let me = unsafe { libc::getuid() };
    let marked = |pid: i32| {
        std::fs::read(format!("/proc/{pid}/environ"))
            .is_ok_and(|env| env.split(|b| *b == 0).any(|v| v == mark.as_bytes()))
    };
    // By uid only in the floored container, whose pid namespace holds nothing else of these
    // uids': never a root daemon on a host, where uid 65534 runs other people's services.
    let left = |pid: i32, uid: u32| match root() && crate::egress::bridge() {
        true => uid == who.uid(),
        false => uid == me && pid != std::process::id() as i32 && marked(pid),
    };
    // Again while something was found: a process may fork as it is killed.
    for _ in 0..3 {
        if crate::procs::kill_all(left) == 0 {
            break;
        }
    }
}

/// The one turn `who`'s programs run in: for notes, transcription and speech alike; for a
/// call, its whole length. `sweep` kills every process of `who`'s uid, so no other run of
/// theirs may be going. When the turn ends, also by its future being dropped mid-run (the
/// adapter's task aborted, a timeout around it), what it left is swept before the next one.
struct Turn(
    #[allow(dead_code)] tokio::sync::SemaphorePermit<'static>,
    Who,
);

impl Turn {
    async fn take(who: Who) -> Turn {
        static NOTE: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        static CALL: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let one = match who {
            Who::Note => &NOTE,
            Who::Call => &CALL,
        };
        Turn(one.acquire().await.expect("never closed"), who)
    }
}

impl Drop for Turn {
    fn drop(&mut self) {
        sweep(self.1);
    }
}

/// `prog` as the decoders run: as `who`'s uid (when the daemon is root: decision 2 of the lane's
/// plan) with a cleared environment, in `/`, in its own process group, at nice 10, each of `fds`
/// at its number, its stdout and stderr piped.
fn command(
    prog: &Path,
    args: &[String],
    fds: &[(&Mem, i32)],
    stdin: Stdio,
    who: Who,
) -> tokio::process::Command {
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
        cmd.uid(who.uid()).gid(who.uid());
    } else {
        // No uid to tell the run's processes by (a test, a daemon that is not root): the
        // sweep finds them by this instead.
        let (name, value) = who.mark();
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
    cmd
}

/// Runs `prog` as `command` does, and kills the group at `limit`.
async fn run(
    prog: &Path,
    args: &[String],
    fds: &[(&Mem, i32)],
    stdin: Stdio,
    limit: Duration,
    who: Who,
) -> Result<Out, Stop> {
    let mut child = command(prog, args, fds, stdin, who).spawn().map_err(|e| {
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
    let _turn = Turn::take(Who::Note).await;
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
    let decoded = run(
        &ffmpeg,
        &args,
        &[(&input, 3)],
        Stdio::null(),
        FFMPEG_LIMIT,
        Who::Note,
    )
    .await?;
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
    whisper_with(cfg, wav, MODEL, lang, Who::Note).await
}

/// One whisper run of `wav` as `lang`, with `model`.
async fn whisper_with(
    cfg: &Config,
    wav: &Mem,
    model: &str,
    lang: &str,
    who: Who,
) -> Result<Out, Stop> {
    let prog = cfg.voice_dir.join("bin/whisper-cli");
    let args = whisper_args(&cfg.voice_dir, model, lang);
    let out = run(
        &prog,
        &args,
        &[(wav, 3)],
        Stdio::null(),
        cfg.voice_wait,
        who,
    )
    .await?;
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
    let _turn = Turn::take(Who::Note).await;
    let made = async {
        let said = mem(Mem::sealed(text.as_bytes()))?;
        let stdin = Stdio::from(mem(said.0.try_clone())?);
        let python = dir.join("py/bin/python");
        let piper = run(
            &python,
            &piper_args(&voice),
            &[],
            stdin,
            SPEAK_LIMIT,
            Who::Note,
        )
        .await?;
        // Piper's stderr is never empty (onnxruntime warns): the exit code and the PCM count.
        if !piper.ok || piper.stdout.is_empty() {
            return Err(Stop::Failed);
        }
        let pcm = mem(Mem::sealed(&piper.stdout))?;
        let args = encode_args(format_for(channel).1);
        let ffmpeg = dir.join("bin/ffmpeg");
        let out = run(
            &ffmpeg,
            &args,
            &[(&pcm, 3)],
            Stdio::null(),
            SPEAK_LIMIT,
            Who::Note,
        )
        .await?;
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

/// One utterance of a phone call (DESIGN §4.26) as whisper reads it: 8 kHz samples doubled to
/// the 16 kHz mono WAV it takes, each new sample halfway between its neighbours.
fn call_wav(pcm: &[i16]) -> Vec<u8> {
    let bytes = (pcm.len() * 4) as u32;
    let mut wav = Vec::with_capacity(WAV_HEADER as usize + bytes as usize);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + bytes).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    // PCM, one channel, 16 kHz, 32000 bytes a second, 2 bytes a frame, 16 bits.
    for field in [
        16u32,
        0x0001_0001,
        16000,
        WAV_BYTES_PER_SEC as u32,
        0x0010_0002,
    ] {
        wav.extend_from_slice(&field.to_le_bytes());
    }
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&bytes.to_le_bytes());
    for (i, s) in pcm.iter().enumerate() {
        let next = pcm.get(i + 1).unwrap_or(s);
        let between = ((i32::from(*s) + i32::from(*next)) / 2) as i16;
        wav.extend_from_slice(&s.to_le_bytes());
        wav.extend_from_slice(&between.to_le_bytes());
    }
    wav
}

/// What the other party said in one utterance of a phone call, 8 kHz samples the daemon cut
/// from the provider's audio itself, in the turn the caller holds: one whisper-cli run on a
/// sealed memfd. `lang` is the call's language, so nothing is detected. [`Call::hear`] runs it
/// when its warm whisper-server is gone.
async fn hear_once(cfg: &Config, pcm: &[i16], lang: &str, who: Who) -> Heard {
    let heard = async {
        let wav = mem(Mem::sealed(&call_wav(pcm)))?;
        let out = whisper_with(cfg, &wav, call_model(cfg), lang, who).await?;
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    };
    call_heard(heard.await, lang)
}

/// What whisper wrote for an utterance, as a `Heard`.
fn call_heard(text: Result<String, Stop>, lang: &str) -> Heard {
    match text.as_deref().map(str::trim) {
        Ok("" | "[BLANK_AUDIO]") => Heard::Silence,
        Ok(text) => Heard::Said {
            lang: lang.into(),
            text: clean(text),
        },
        Err(Stop::Failed) => Heard::Failed,
        Err(Stop::TimedOut) => Heard::TimedOut,
    }
}

/// The model a call's utterances are transcribed with: the smaller one when the image has it.
pub fn call_model(cfg: &Config) -> &'static str {
    match cfg.voice_dir.join(CALL_MODEL).exists() {
        true => CALL_MODEL,
        false => MODEL,
    }
}

/// ffmpeg's argv to turn Piper's PCM on fd 3 into what a phone line carries: G.711 μ-law at
/// 8 kHz, raw, on stdout.
fn call_args() -> Vec<String> {
    let args = "-hide_banner -nostdin -loglevel error -f s16le -ar";
    (args.split(' '))
        .chain([PIPER_RATE, "-ac", "1", "-i", "/proc/self/fd/3"])
        .chain([
            "-ar",
            "8000",
            "-ac",
            "1",
            "-c:a",
            "pcm_mulaw",
            "-f",
            "mulaw",
            "pipe:1",
        ])
        .map(String::from)
        .collect()
}

fn call_voice_path(cfg: &Config, lang: &str) -> std::path::PathBuf {
    cfg.voice_dir
        .join(if lang == "ko" { VOICE_KO } else { VOICE_EN })
}

/// One line of a phone call, speakable `text` in `lang`, as μ-law at 8 kHz, in the turn the
/// caller holds: one Piper run, then `mulaw`. [`Call::say`] runs it when its warm Piper is gone.
async fn say_once(cfg: &Config, text: &str, lang: &str, who: Who) -> Option<Vec<u8>> {
    let made = async {
        let said = mem(Mem::sealed(text.as_bytes()))?;
        let stdin = Stdio::from(mem(said.0.try_clone())?);
        let python = cfg.voice_dir.join("py/bin/python");
        let args = piper_args(&call_voice_path(cfg, lang));
        let piper = run(&python, &args, &[], stdin, SPEAK_LIMIT, who).await?;
        match piper.ok {
            true => Ok(piper.stdout),
            false => Err(Stop::Failed),
        }
    };
    mulaw(cfg, made.await, who).await
}

/// Piper's PCM for a line of a call as μ-law at 8 kHz: one ffmpeg run, as `who`'s.
async fn mulaw(cfg: &Config, pcm: Result<Vec<u8>, Stop>, who: Who) -> Option<Vec<u8>> {
    let made = async {
        let pcm = pcm?;
        if pcm.is_empty() {
            return Err(Stop::Failed);
        }
        let pcm = mem(Mem::sealed(&pcm))?;
        let ffmpeg = cfg.voice_dir.join("bin/ffmpeg");
        let (args, fds) = (call_args(), [(&pcm, 3)]);
        let out = run(&ffmpeg, &args, &fds, Stdio::null(), SPEAK_LIMIT, who).await?;
        match out.ok {
            true => Ok(out.stdout),
            false => Err(Stop::Failed),
        }
    };
    match made.await {
        Ok(audio) if !audio.is_empty() => Some(audio),
        _ => {
            eprintln!("zephyr: voice: a line of a call could not be spoken");
            None
        }
    }
}

/// Piper kept loaded for a call: one JSON string a line on stdin; for each, its PCM (s16le mono
/// at `PIPER_RATE`) on stdout after its length, 4 bytes little-endian (`ask_mouth`). Piper 1.8's
/// own `--output-raw` also keeps the voice loaded across stdin's lines, but writes nothing
/// between one line's audio and the next, so the daemon could not tell where a line ends.
const PIPER_LINES: &str = "import json, struct, sys
from piper import PiperVoice
voice = PiperVoice.load(sys.argv[1])
out = sys.stdout.buffer
for line in sys.stdin:
    texts = [t for t in json.loads(line).splitlines() if t.strip()]
    pcm = b''.join(c.audio_int16_bytes for t in texts for c in voice.synthesize(t))
    out.write(struct.pack('<I', len(pcm)) + pcm)
    out.flush()
";

/// The folder of the call's whisper-server socket, in `<state>-public` (root's, 0755). Made by
/// root as root's, group `CALL_UID`, mode 0730 (`start_ear`): the server can bind in it, root
/// can reach the socket, and neither the agent's uid nor `nobody` can enter it. The call uid can
/// write in it, so the daemon never follows what is there (`socket_fd`).
fn socket_dir(cfg: &Config) -> std::path::PathBuf {
    crate::drafts::public_dir(cfg).join("voice")
}

/// whisper-server's argv: `whisper_args` but for the file, with its one address the unix
/// socket `sock` (the image's build binds `--host` as a path, `deploy/Dockerfile`), never a
/// port. The call's language and model are the server's for its whole life.
fn server_args(dir: &Path, model: &str, lang: &str, sock: &Path) -> Vec<String> {
    let mut args = whisper_args(dir, model, lang);
    args.truncate(args.len() - 2);
    args.extend(["--host".into(), sock.to_string_lossy().into_owned()]);
    args
}

/// The server's socket, opened without following anything in its place: it must be a socket
/// of the call uid's, else a server that had been taken over could point the daemon elsewhere
/// with a symlink. Connected through `/proc/self/fd/N`, so what was checked is what is reached.
fn socket_fd(sock: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_NOFOLLOW)
        .open(sock)?;
    let meta = file.metadata()?;
    match meta.file_type().is_socket() && meta.uid() == Who::Call.runs_as() {
        true => Ok(file),
        false => Err(std::io::Error::other("not the call's socket")),
    }
}

/// One of a call's warm processes; its group is killed when it goes, unless it was already
/// reaped (`end`): until then its pid is still its group's.
struct Warm(tokio::process::Child);

impl Warm {
    fn pid(&self) -> i32 {
        self.0.id().expect("never waited on") as i32
    }

    /// Whether it has exited, seen without reaping it.
    fn exited(&self) -> bool {
        // Safety: `info` is plain data the call fills in; the pid is our unreaped child's.
        let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
        let flags = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
        let r = unsafe { libc::waitid(libc::P_PID, self.pid() as libc::id_t, &mut info, flags) };
        r != 0 || unsafe { info.si_pid() } != 0
    }

    /// Kills its group and waits for it to be reaped.
    async fn end(mut self) {
        // Safety: the group is the child's own and the child is not reaped yet.
        unsafe { libc::killpg(self.pid(), libc::SIGKILL) };
        let _ = self.0.wait().await;
    }
}

impl Drop for Warm {
    fn drop(&mut self) {
        // Not after `end`: a reaped child's pid may be another process's by now.
        if let Some(pid) = self.0.id() {
            // Safety: the group is the child's own and the child is not reaped.
            unsafe { libc::killpg(pid as i32, libc::SIGKILL) };
        }
    }
}

/// `command` for a warm process: also killed by the kernel if the thread that started it goes
/// (`PR_SET_PDEATHSIG`; set after std's setuid, which would clear it), and refused if that
/// thread was already gone.
fn warm_command(prog: &Path, args: &[String], stdin: Stdio) -> tokio::process::Command {
    let mut cmd = command(prog, args, &[], stdin, Who::Call);
    let parent = std::process::id() as i32;
    // Safety: prctl and getppid are async-signal-safe and allocate nothing.
    unsafe {
        cmd.pre_exec(move || {
            if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            match libc::getppid() == parent {
                true => Ok(()),
                false => Err(std::io::Error::from_raw_os_error(libc::ESRCH)),
            }
        });
    }
    cmd
}

/// One phone call's voice with `calls.speech = "local"` (DESIGN §4.26): one whisper-server and
/// one Piper, each loaded once for the call, so a turn costs the models' time and not a
/// process's start. Both run as `CALL_UID` (`command`: its own process group, no network under
/// the floor); the server's only address is a unix socket in `socket_dir`, Piper's its pipes.
/// The call holds the call uid's `Turn` from its first use (`warm`) to its drop, so nothing else
/// of that uid's runs beside them; voice notes have `nobody` and a turn of their own, and never
/// wait on a call. `kill` (the call's end, a hang-up, an error) and the drop (its future
/// dropped) kill both groups, and the call uid's sweep takes whatever they left. Either one
/// failing gives way, for the rest of the call, to the per-utterance path (`hear_once`,
/// `say_once`, run as the call uid too).
pub struct Call {
    cfg: Config,
    lang: String,
    turn: tokio::sync::OnceCell<Turn>,
    ear: tokio::sync::Mutex<Option<Warm>>,
    mouth: tokio::sync::Mutex<Option<Warm>>,
}

impl Call {
    /// Starts nothing yet: that is `warm`'s, or the first utterance's.
    pub fn new(cfg: &Config, lang: &str) -> Call {
        Call {
            cfg: cfg.clone(),
            lang: lang.into(),
            turn: Default::default(),
            ear: Default::default(),
            mouth: Default::default(),
        }
    }

    /// Takes the call uid's turn and starts the pair, once; they load while the call rings.
    pub async fn warm(&self) {
        self.turn
            .get_or_init(|| async {
                let turn = Turn::take(Who::Call).await;
                // What a daemon before this one left (killed mid-call): nothing of the call uid's
                // runs into a new call.
                sweep(Who::Call);
                *self.ear.lock().await = self.start_ear();
                *self.mouth.lock().await = self.start_mouth();
                turn
            })
            .await;
    }

    /// Ends the pair now, and returns once it is gone: both groups killed and reaped, then the
    /// call uid's sweep, and the socket's folder removed.
    pub async fn kill(&self) {
        for side in [&self.ear, &self.mouth] {
            if let Some(warm) = side.lock().await.take() {
                warm.end().await;
            }
        }
        sweep(Who::Call);
        let _ = std::fs::remove_dir_all(socket_dir(&self.cfg));
    }

    fn start_ear(&self) -> Option<Warm> {
        use std::os::unix::{fs::DirBuilderExt, process::CommandExt};
        let (cfg, dir) = (&self.cfg, socket_dir(&self.cfg));
        let prog = cfg.voice_dir.join("bin/whisper-server");
        if !prog.exists() {
            return None;
        }
        let started = (|| {
            // What a call before this one left, if the daemon died mid-call.
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(dir.parent().expect("in <state>-public"))?;
            if root() {
                // The daemon holds no CAP_CHOWN (nor DAC_OVERRIDE): a mkdir run with the call
                // uid's gid gives the folder that group, and root owns it.
                let made = std::process::Command::new("/usr/bin/mkdir")
                    .args(["-m", "0730", "--"])
                    .arg(&dir)
                    .env_clear()
                    .gid(CALL_UID)
                    .status()?;
                if !made.success() {
                    return Err(std::io::Error::other("mkdir failed"));
                }
            } else {
                std::fs::DirBuilder::new().mode(0o700).create(&dir)?;
            }
            let sock = dir.join("whisper.sock");
            let args = server_args(&cfg.voice_dir, call_model(cfg), &self.lang, &sock);
            let mut cmd = warm_command(&prog, &args, Stdio::null());
            // srw-rw-rw-: root, which is neither its owner nor in its group, may connect; the
            // folder keeps everyone else out.
            // Safety: umask is async-signal-safe and cannot fail.
            unsafe {
                cmd.pre_exec(|| {
                    libc::umask(0o111);
                    Ok(())
                });
            }
            cmd.stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        })();
        match started {
            Ok(child) => Some(Warm(child)),
            Err(e) => {
                eprintln!("zephyr: voice: starting the call's whisper-server: {e}");
                None
            }
        }
    }

    fn start_mouth(&self) -> Option<Warm> {
        if !call_voice(&self.cfg, &self.lang) {
            return None;
        }
        let python = self.cfg.voice_dir.join("py/bin/python");
        let voice = call_voice_path(&self.cfg, &self.lang);
        let args = ["-I", "-c", PIPER_LINES, &voice.to_string_lossy()].map(String::from);
        let mut cmd = warm_command(&python, &args, Stdio::piped());
        match cmd.stderr(Stdio::null()).spawn() {
            Ok(child) => Some(Warm(child)),
            Err(e) => {
                eprintln!("zephyr: voice: starting the call's Piper: {e}");
                None
            }
        }
    }

    /// What the other party said in one utterance (`hear_once`), from the warm whisper-server
    /// while it lasts.
    pub async fn hear(&self, pcm: &[i16]) -> Heard {
        if !floored(&self.cfg) {
            return Heard::NoFloor;
        }
        self.warm().await;
        let mut ear = self.ear.lock().await;
        if let Some(server) = ear.as_ref() {
            let asked = tokio::time::timeout(self.cfg.voice_wait, self.ask_ear(server, pcm));
            let stop = match asked.await {
                Ok(Ok(text)) => return call_heard(Ok(text), &self.lang),
                Ok(Err(stop)) => stop,
                Err(_) => Stop::TimedOut,
            };
            *ear = None;
            eprintln!(
                "zephyr: voice: the call's whisper-server is gone; each utterance runs whisper"
            );
            if let Stop::TimedOut = stop {
                return Heard::TimedOut;
            }
        }
        drop(ear);
        hear_once(&self.cfg, pcm, &self.lang, Who::Call).await
    }

    /// One `POST /inference` of the utterance's WAV over the server's socket (multipart, its
    /// `file` and `response_format=text`, `Connection: close`), waiting for the socket while
    /// the server loads its model; the reply's body is the text, under `HTTP/1.1 200`.
    async fn ask_ear(&self, server: &Warm, pcm: &[i16]) -> Result<String, Stop> {
        use std::os::fd::AsRawFd;
        use tokio::io::AsyncWriteExt;
        let sock = socket_dir(&self.cfg).join("whisper.sock");
        let mut stream = loop {
            let fd = match socket_fd(&sock) {
                Ok(fd) => fd,
                // Not there yet while it loads; something else in its place is refused.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound && !server.exited() => {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    continue;
                }
                Err(_) => return Err(Stop::Failed),
            };
            let path = format!("/proc/self/fd/{}", fd.as_raw_fd());
            match tokio::net::UnixStream::connect(&path).await {
                Ok(stream) => break stream,
                Err(_) if !server.exited() => tokio::time::sleep(Duration::from_millis(20)).await,
                Err(_) => return Err(Stop::Failed),
            }
        };
        const B: &str = "zephyr-voice-6f1d2c9a8e3b4a57";
        let mut body = format!(
            "--{B}\r\nContent-Disposition: form-data; name=\"response_format\"\r\n\r\ntext\r\n\
             --{B}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\n\
             Content-Type: audio/wav\r\n\r\n"
        )
        .into_bytes();
        body.extend(call_wav(pcm));
        body.extend(format!("\r\n--{B}--\r\n").into_bytes());
        let head = format!(
            "POST /inference HTTP/1.1\r\nHost: whisper\r\nConnection: close\r\n\
             Content-Type: multipart/form-data; boundary={B}\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        let failed = |_| Stop::Failed;
        stream.write_all(head.as_bytes()).await.map_err(failed)?;
        stream.write_all(&body).await.map_err(failed)?;
        let mut reply = Vec::new();
        (stream.take(MAX_OUT).read_to_end(&mut reply).await).map_err(failed)?;
        let reply = String::from_utf8_lossy(&reply);
        match reply.split_once("\r\n\r\n") {
            Some((head, text)) if head.starts_with("HTTP/1.1 200 ") => Ok(text.into()),
            _ => Err(Stop::Failed),
        }
    }

    /// One line of the call spoken in its language as μ-law at 8 kHz (`say_once`), by the warm
    /// Piper while it lasts, or `None`: no voice for the language in this image, nothing
    /// speakable in `text`, or Piper or ffmpeg failed.
    pub async fn say(&self, text: &str) -> Option<Vec<u8>> {
        if !floored(&self.cfg) || !call_voice(&self.cfg, &self.lang) {
            return None;
        }
        let text = speakable(text)?;
        self.warm().await;
        let mut mouth = self.mouth.lock().await;
        if let Some(piper) = mouth.as_mut() {
            match tokio::time::timeout(SPEAK_LIMIT, ask_mouth(piper, &text)).await {
                Ok(Ok(pcm)) => return mulaw(&self.cfg, Ok(pcm), Who::Call).await,
                _ => *mouth = None,
            }
            eprintln!("zephyr: voice: the call's Piper is gone; each line runs Piper");
        }
        drop(mouth);
        say_once(&self.cfg, &text, &self.lang, Who::Call).await
    }
}

/// One line through the warm Piper (`PIPER_LINES`).
async fn ask_mouth(piper: &mut Warm, text: &str) -> std::io::Result<Vec<u8>> {
    use tokio::io::AsyncWriteExt;
    let (stdin, stdout) = (piper.0.stdin.as_mut(), piper.0.stdout.as_mut());
    let (Some(stdin), Some(stdout)) = (stdin, stdout) else {
        return Err(std::io::ErrorKind::BrokenPipe.into());
    };
    let line = serde_json::to_string(text).expect("a string") + "\n";
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    let len = u64::from(stdout.read_u32_le().await?);
    if len > MAX_OUT {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut pcm = vec![0; len as usize];
    stdout.read_exact(&mut pcm).await?;
    Ok(pcm)
}

impl Drop for Call {
    fn drop(&mut self) {
        // Also a pair `warm` started but did not finish (its future dropped mid-way).
        let started = self.ear.get_mut().take().is_some();
        self.mouth.get_mut().take();
        if started {
            let _ = std::fs::remove_dir_all(socket_dir(&self.cfg));
        }
        // Then `turn` goes, and with it the call uid's sweep.
    }
}

/// The uid a voice note's decoder gets: `/usr/bin/id -u`, run as they are. For `doctor` and the
/// in-image test.
pub async fn uid() -> Option<u32> {
    let _turn = Turn::take(Who::Note).await;
    uid_of(Who::Note).await
}

/// The uid a call's whisper and Piper get, the same way. Not under the call's turn, which a
/// call in progress holds; `id` exits at once and its group goes with it.
pub async fn call_uid() -> Option<u32> {
    uid_of(Who::Call).await
}

async fn uid_of(who: Who) -> Option<u32> {
    let id = Path::new("/usr/bin/id");
    let out = run(id, &["-u".into()], &[], Stdio::null(), FFMPEG_LIMIT, who).await;
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
        lines.push(match call_uid().await {
            Some(CALL_UID) => "ok   voice: a call's whisper and Piper run as uid 65533".into(),
            other => {
                format!("warn voice: a call's whisper and Piper run as {other:?}, not uid 65533")
            }
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
        for file in [
            "bin/ffmpeg",
            "bin/whisper-cli",
            "bin/whisper-server",
            "py/bin/python",
        ] {
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

    #[tokio::test]
    async fn a_call_is_heard_from_8_khz_samples_as_nobody() {
        let cfg = fakes("voice-call-hear", "");
        // As `Call::hear` runs it: in the call uid's turn, which no other call's sweep cuts.
        let _turn = Turn::take(Who::Call).await;
        mark(&cfg, "say", "Seven is full, we have seven thirty");
        let pcm: Vec<i16> = (0..8000)
            .map(|i| if i / 20 % 2 == 0 { 9000 } else { -9000 })
            .collect();
        let heard = hear_once(&cfg, &pcm, "en", Who::Call).await;
        assert_eq!(heard, said("en", "Seven is full, we have seven thirty"));
        // One whisper run, no ffmpeg: on the WAV the daemon made, in the call's language.
        assert!(log(&cfg, "ffmpeg").is_empty());
        let runs = log(&cfg, "whisper-cli");
        assert_eq!(runs.len(), 1);
        let wav = call_wav(&pcm);
        assert_eq!(runs[0]["sha256"], sha256(&wav));
        assert_eq!(
            (&wav[..4], &wav[36..40], wav.len()),
            (&b"RIFF"[..], &b"data"[..], 44 + 32000)
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16000);
        let argv: Vec<&str> = (runs[0]["argv"].as_array().unwrap().iter())
            .map(|a| a.as_str().unwrap())
            .collect();
        let lang = argv.iter().position(|a| *a == "-l").unwrap();
        assert_eq!(argv[lang + 1], "en");
        let model = argv.iter().position(|a| *a == "-m").unwrap();
        assert!(
            argv[model + 1].ends_with("models/ggml-small-q5_1.bin"),
            "{argv:?}"
        );
        // With the smaller model in the image, a call's utterances go to it; a voice note's
        // still go to the small one.
        std::fs::create_dir_all(cfg.voice_dir.join("models")).unwrap();
        std::fs::write(cfg.voice_dir.join(CALL_MODEL), "").unwrap();
        assert_eq!(call_model(&cfg), CALL_MODEL);
        hear_once(&cfg, &pcm, "en", Who::Call).await;
        let last = log(&cfg, "whisper-cli").pop().unwrap();
        assert!(
            last["argv"]
                .to_string()
                .contains("models/ggml-base-q5_1.bin"),
            "{last}"
        );
        transcribe(&cfg, b"a voice note").await;
        let last = log(&cfg, "whisper-cli").pop().unwrap();
        assert!(
            last["argv"]
                .to_string()
                .contains("models/ggml-small-q5_1.bin"),
            "{last}"
        );
        // Each utterance takes the next line of what the other party says.
        mark(&cfg, "says", "first\nsecond\n");
        assert_eq!(
            hear_once(&cfg, &pcm, "en", Who::Call).await,
            said("en", "first")
        );
        assert_eq!(
            hear_once(&cfg, &pcm, "en", Who::Call).await,
            said("en", "second")
        );
        mark(&cfg, "say", "");
        std::fs::remove_file(cfg.voice_dir.join("says")).unwrap();
        assert_eq!(hear_once(&cfg, &pcm, "en", Who::Call).await, Heard::Silence);
    }

    #[tokio::test]
    async fn a_call_line_is_spoken_as_mu_law() {
        let cfg = fakes("voice-call-say", "");
        let call = Call::new(&cfg, "en");
        let audio = call.say("Seven thirty, **please**.").await.unwrap();
        // Only what is spoken goes to Piper; the stand-ins pass it through: Piper's `pcm:`,
        // then ffmpeg's `ulaw:`.
        assert_eq!(audio, b"ulaw:pcm:Seven thirty, please.");
        let ffmpeg = &log(&cfg, "ffmpeg")[0];
        let argv = ffmpeg["argv"].to_string();
        for part in [
            "\"-f\",\"mulaw\"",
            "\"-ar\",\"8000\"",
            "\"-ar\",\"22050\"",
            "pcm_mulaw",
        ] {
            assert!(argv.contains(part), "{part} in {argv}");
        }
        // No voice for the language: nothing is said, and nothing runs.
        assert_eq!(Call::new(&cfg, "ko").say("안녕하세요.").await, None);
        assert_eq!(Call::new(&cfg, "fr").say("Bonjour.").await, None);
        assert_eq!(log(&cfg, "python").len(), 1);
        // Piper fails, warm and then once: nothing is said.
        mark(&cfg, "piper_fail", "");
        assert_eq!(call.say("Hello.").await, None);
    }

    /// One second of an 8 kHz square wave: an utterance as the call's endpointer cuts it.
    fn tone() -> Vec<i16> {
        (0..8000)
            .map(|i| if i / 20 % 2 == 0 { 9000 } else { -9000 })
            .collect()
    }

    /// Waits until the fake has written its pid to `file`.
    async fn started(cfg: &Config, file: &str) {
        let path = cfg.voice_dir.join(file);
        for _ in 0..400 {
            if std::fs::read_to_string(&path).is_ok_and(|s| !s.is_empty()) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        panic!("{file} was never written");
    }

    /// A call keeps one whisper-server and one Piper loaded (DESIGN §4.26): its second
    /// utterance and its second line start neither again, and when the call goes, so do they.
    #[tokio::test]
    async fn a_calls_second_utterance_starts_no_process() {
        let cfg = fakes("voice-call-warm", "");
        mark(&cfg, "says", "first\nsecond\n");
        let call = Call::new(&cfg, "en");
        let pcm = tone();
        assert_eq!(call.hear(&pcm).await, said("en", "first"));
        assert_eq!(call.hear(&pcm).await, said("en", "second"));
        let line = call.say("Seven thirty, **please**.").await.unwrap();
        assert_eq!(line, b"ulaw:pcm:Seven thirty, please.");
        let line = call.say("Thank you.\nGoodbye.").await.unwrap();
        assert_eq!(line, b"ulaw:pcm:Thank you.\nGoodbye.");
        let (server, piper) = (log(&cfg, "whisper-server"), log(&cfg, "python"));
        assert_eq!((server.len(), piper.len()), (2, 2), "{server:?} {piper:?}");
        assert_eq!(server[0]["pid"], server[1]["pid"]);
        assert_eq!(piper[0]["pid"], piper[1]["pid"]);
        assert!(log(&cfg, "whisper-cli").is_empty());
        // Given the WAV the daemon made, and the text exactly as checked for the facts.
        assert_eq!(server[0]["sha256"], sha256(&call_wav(&pcm)));
        assert_eq!(piper[0]["sha256"], sha256(b"Seven thirty, please."));
        // Run as the decoders are, and on no port: the server's one address is a socket in a
        // 0700 folder of its own.
        let uid = match root() {
            true => NOBODY,
            false => unsafe { libc::geteuid() },
        };
        let sock = socket_dir(&cfg).join("whisper.sock");
        for run in [&server[0], &piper[0]] {
            assert_eq!(run["uid"], uid);
            assert!(!run["env"].to_string().contains("HOME"), "{run}");
        }
        let argv = server[0]["argv"].to_string();
        assert!(argv.contains(&format!(r#""--host",{:?}"#, sock)), "{argv}");
        assert!(!argv.contains("--port") && argv.contains("ggml-small-q5_1.bin"));
        assert!(
            argv.contains(r#""-l","en""#) && argv.contains(r#""--vad""#),
            "{argv}"
        );
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(socket_dir(&cfg))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, if root() { 0o730 } else { 0o700 });
        let piper_argv = piper[0]["argv"].to_string();
        assert!(
            piper_argv.contains("en_US-lessac-medium.onnx"),
            "{piper_argv}"
        );
        drop(call);
        gone(&cfg, "whisper-server.pid").await;
        gone(&cfg, "piper.pid").await;
        assert!(!socket_dir(&cfg).exists());
    }

    /// Either of the pair dying mid-call: that side goes back to a process an utterance.
    #[tokio::test]
    async fn a_dead_server_or_piper_falls_back_to_a_run_each() {
        let cfg = fakes("voice-call-dead", "");
        let call = Call::new(&cfg, "en");
        let pcm = tone();
        assert_eq!(call.hear(&pcm).await, said("en", "hello from the fake"));
        assert_eq!(call.say("Hello.").await.unwrap(), b"ulaw:pcm:Hello.");
        for file in ["whisper-server.pid", "piper.pid"] {
            let pid = std::fs::read_to_string(cfg.voice_dir.join(file)).unwrap();
            unsafe { libc::kill(pid.trim().parse().unwrap(), libc::SIGKILL) };
            gone(&cfg, file).await;
        }
        assert_eq!(call.hear(&pcm).await, said("en", "hello from the fake"));
        assert_eq!(call.hear(&pcm).await, said("en", "hello from the fake"));
        assert_eq!(call.say("Goodbye.").await.unwrap(), b"ulaw:pcm:Goodbye.");
        assert_eq!(log(&cfg, "whisper-server").len(), 1);
        let cli = log(&cfg, "whisper-cli");
        assert_eq!(cli.len(), 2);
        assert!(cli[0]["argv"].to_string().contains(r#""-l","en""#));
        let piper = log(&cfg, "python");
        assert_eq!(piper.len(), 2);
        assert!(piper[1]["argv"].to_string().contains("--output-raw"));
        assert_eq!(piper[1]["sha256"], sha256(b"Goodbye."));
    }

    /// The call's future dropped mid-utterance (the daemon stopping, the call's task aborted):
    /// the pair goes with it, and the voice is free for the next note.
    #[tokio::test]
    async fn a_dropped_call_leaves_nothing_running() {
        let cfg = fakes("voice-call-dropped", "");
        mark(&cfg, "hang", "");
        let call = std::sync::Arc::new(Call::new(&cfg, "en"));
        let theirs = call.clone();
        let run = tokio::spawn(async move { theirs.hear(&tone()).await });
        started(&cfg, "hang.pid").await;
        started(&cfg, "piper.pid").await;
        drop(call);
        run.abort();
        assert!(run.await.unwrap_err().is_cancelled());
        gone(&cfg, "whisper-server.pid").await;
        gone(&cfg, "piper.pid").await;
        std::fs::remove_file(cfg.voice_dir.join("hang")).unwrap();
        assert_eq!(
            transcribe(&cfg, b"next").await.0,
            said("en", "hello from the fake")
        );
    }

    /// A voice note while a call is on is heard at once: the call's pair has a uid and a lock of
    /// its own, and the note's sweep leaves it alone.
    #[tokio::test]
    async fn a_note_during_a_call_is_heard_at_once() {
        let cfg = fakes("voice-call-note", "");
        let call = Call::new(&cfg, "en");
        call.warm().await;
        started(&cfg, "whisper-server.pid").await;
        started(&cfg, "piper.pid").await;
        let note = tokio::time::timeout(Duration::from_secs(10), transcribe(&cfg, b"a note"));
        let (heard, _) = note.await.expect("the note waited for the call");
        assert_eq!(heard, said("en", "hello from the fake"));
        // The pair is still up and answering, not swept with the note's run.
        assert_eq!(call.hear(&tone()).await, said("en", "hello from the fake"));
        assert_eq!(call.say("Hello.").await.unwrap(), b"ulaw:pcm:Hello.");
        assert_eq!(log(&cfg, "whisper-server").len(), 1);
        let piper = log(&cfg, "python");
        assert!(piper
            .iter()
            .any(|p| p["argv"].to_string().contains("\"-c\"")));
        // Each kind of run carries its own mark, so neither sweep can take the other's.
        if !root() {
            let env = log(&cfg, "whisper-server")[0]["env"].to_string();
            assert!(env.contains("ZEPHYR_VOICE_CALL") && !env.contains("ZEPHYR_VOICE_RUN"));
            let env = log(&cfg, "ffmpeg")[0]["env"].to_string();
            assert!(env.contains("ZEPHYR_VOICE_RUN"), "the note's decode: {env}");
        }
    }

    /// The socket's folder is the call uid's to write: a symlink in the socket's place is never
    /// followed, and the utterance goes to whisper-cli instead.
    #[tokio::test]
    async fn a_symlink_in_the_sockets_place_is_refused() {
        let mut cfg = fakes("voice-call-symlink", "");
        cfg.voice_wait = Duration::from_secs(2);
        let call = Call::new(&cfg, "en");
        assert_eq!(call.hear(&tone()).await, said("en", "hello from the fake"));
        let dir = socket_dir(&cfg);
        let decoy = std::os::unix::net::UnixListener::bind(dir.join("decoy.sock")).unwrap();
        decoy.set_nonblocking(true).unwrap();
        std::fs::remove_file(dir.join("whisper.sock")).unwrap();
        std::os::unix::fs::symlink("decoy.sock", dir.join("whisper.sock")).unwrap();
        assert_eq!(call.hear(&tone()).await, said("en", "hello from the fake"));
        let accepted = decoy.accept().map(|_| ());
        let refused = accepted.is_err_and(|e| e.kind() == std::io::ErrorKind::WouldBlock);
        assert!(refused, "the daemon connected through the symlink");
        assert_eq!(log(&cfg, "whisper-server").len(), 1);
        assert_eq!(log(&cfg, "whisper-cli").len(), 1);
    }

    /// What a daemon before this one left of the call uid's (a crash mid-call) is killed before
    /// a new pair starts.
    #[tokio::test]
    async fn a_leftover_of_the_call_uid_is_killed_before_a_new_pair() {
        let cfg = fakes("voice-call-leftover", "");
        let (name, value) = Who::Call.mark();
        let mut left = std::process::Command::new("/bin/sleep");
        let mut left = left.arg("600").env(name, value).spawn().unwrap();
        let call = Call::new(&cfg, "en");
        call.warm().await;
        let reaped = tokio::task::spawn_blocking(move || left.wait());
        let ended = tokio::time::timeout(Duration::from_secs(5), reaped).await;
        let status = ended
            .expect("the leftover is still running")
            .unwrap()
            .unwrap();
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(status.signal(), Some(libc::SIGKILL));
    }

    /// `kill` ends the pair before it returns: both reaped, the folder gone, though the call
    /// itself is still held.
    #[tokio::test]
    async fn kill_ends_the_pair_before_it_returns() {
        let cfg = fakes("voice-call-kill", "");
        let call = Call::new(&cfg, "en");
        assert_eq!(call.hear(&tone()).await, said("en", "hello from the fake"));
        assert_eq!(call.say("Hello.").await.unwrap(), b"ulaw:pcm:Hello.");
        call.kill().await;
        for file in ["whisper-server.pid", "piper.pid"] {
            let pid = std::fs::read_to_string(cfg.voice_dir.join(file)).unwrap();
            let proc = format!("/proc/{}", pid.trim());
            assert!(!Path::new(&proc).exists(), "{file}: still there after kill");
        }
        assert!(!socket_dir(&cfg).exists());
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
