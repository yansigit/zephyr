//! A phone call in progress (DESIGN §4.26): the listener Telnyx reaches, the registry of live
//! calls, and one task per call that joins Telnyx's media socket, whisper, an rpi child with
//! no tools (`call-<id>`) and Piper.
//!
//! Every word the other party hears passes through here as text first: the opening and
//! closing lines are the daemon's own, and each sentence of the child's reply is checked for
//! private facts before Piper is given it. What the other party says is transcribed, written
//! to the transcript, and handed to the child quoted; it reaches nothing else. No audio is
//! kept.
//!
//! The listener answers two paths and nothing else: `GET /calls/<token>` (the media socket: a
//! 32-byte token made for one call, good for one connection between its dial and its end) and
//! `POST /calls/events` (Telnyx's webhooks). Both must carry Telnyx's signature. It binds the
//! container's own address (`egress::listen_addr`), which the agent's uid cannot reach under
//! the network floor, and does not start without the floor.

use super::telnyx::{self, Dial, Frame, Telephony, Telnyx};
use super::{Ask, Endpointer, Sound};
use crate::approvals::FactsMode;
use crate::config::Config;
use crate::rpc::Child;
use crate::scheduler::Wake;
use crate::store::{Action, Store};
use crate::voice::Heard;
use futures_util::stream::FuturesUnordered;
use futures_util::{SinkExt, StreamExt};
use http_body_util::Full;
use hyper::body::{Bytes, Incoming};
use hyper::header::{self, HeaderValue};
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use subtle::ConstantTimeEq;
use tokio::sync::{mpsc, watch};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

type Socket = WebSocketStream<TokioIo<hyper::upgrade::Upgraded>>;

/// Connections the listener holds at once; one more is closed at once.
const MAX_CONNS: usize = 16;
/// How long a frame may take to send on the media socket.
const SEND_WAIT: Duration = Duration::from_secs(10);
/// The longest webhook body read.
const MAX_EVENT: usize = 64 * 1024;
/// The longest media frame taken: Telnyx's are a few hundred bytes.
const MAX_FRAME: usize = 256 * 1024;
/// After the answer, how long Telnyx has to connect the audio.
const SOCKET_WAIT: Duration = Duration::from_secs(10);
/// How long the closing line has to be spoken and played before the hang-up goes anyway.
const CLOSING_WAIT: Duration = Duration::from_secs(30);
/// The closing line starts this long before the cap (or halfway, for a cap under a minute).
const CLOSING_LEAD: Duration = Duration::from_secs(30);
/// The report's transcript is cut here.
const MAX_REPORT: usize = 4000;
/// Audio goes to Telnyx in pieces of a second.
/// ASSUMPTION(live): Telnyx takes chunks of 20 ms to 30 s and plays them back to back.
const CHUNK: usize = 8000;
/// How much of what the agent already said is read with the next sentence, so a private fact
/// said in two halves is still found.
const TAIL: usize = 200;

/// What reaches a call's task from outside it.
enum Event {
    /// Telnyx's media socket, verified by the listener.
    Socket(Box<Socket>),
    /// Telnyx's `call.answered`.
    Answered,
    /// Telnyx's `call.hangup`: the other side ended it.
    TheirHangup,
    /// The owner's `/hangup`, `/stop` or `/pause`.
    Hangup(&'static str),
    /// What whisper made of one utterance.
    Heard(Heard),
    /// One line Piper spoke, or could not; with Telnyx's speech, one it took (no audio).
    Spoken(Say, Option<Vec<u8>>),
    /// Telnyx's `call.speak.ended`: the line it was speaking has played.
    SpeakEnded,
    /// The daemon is stopping: the call is closed properly first.
    Stopping,
}

/// One line for Piper.
struct Say {
    /// `agent`, or `zephyr` for the daemon's own lines.
    who: &'static str,
    text: String,
    /// The closing line: the hang-up follows it.
    closing: bool,
}

struct Slot {
    token: String,
    /// The media socket came: the token is spent.
    taken: bool,
    provider_ref: Option<String>,
    tx: mpsc::UnboundedSender<Event>,
}

/// A call `actions::approve` started, on its way to `run`.
pub struct Start {
    id: i64,
    ask: Ask,
    to: (String, String),
    token: String,
    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,
}

/// The calls in progress. Cloned into the router (`/ok`, `/hangup`, `/stop`) and the listener.
#[derive(Clone)]
pub struct Calls {
    live: Arc<Mutex<HashMap<i64, Slot>>>,
    starts: mpsc::UnboundedSender<Start>,
    /// Webhooks that came for a call whose dial has not answered the daemon yet: Telnyx can
    /// post `call.answered` before its answer to the dial is read. Given to the call once its
    /// id is known (`set_ref`).
    early: Arc<Mutex<Vec<telnyx::Hook>>>,
}

/// The most early webhooks kept.
const MAX_EARLY: usize = 32;

impl Calls {
    /// A registry nothing runs: a start is refused.
    #[cfg(test)]
    pub fn idle() -> Calls {
        Calls::new().0
    }

    /// The registry, and the starts `run` takes.
    pub fn new() -> (Calls, mpsc::UnboundedReceiver<Start>) {
        let (starts, rx) = mpsc::unbounded_channel();
        let calls = Calls {
            live: Arc::default(),
            starts,
            early: Arc::default(),
        };
        (calls, rx)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<i64, Slot>> {
        self.live.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Starts approved call `a` (the owner's `/ok`), checked once more: the number, the hours
    /// as they are now, the provider, the listener. `Ok` is what the owner is told; `Err` why
    /// nothing was dialled.
    pub fn start(&self, cfg: &Config, a: &Action, to: (String, String)) -> Result<String, String> {
        let args: Value = serde_json::from_str(&a.payload).unwrap_or_default();
        let ask = super::check(cfg, &args, cfg.calls_clock())
            .map_err(|e| e.trim_start_matches("not run: ").to_string())?;
        let calls = cfg.calls();
        if calls.port.is_none() || calls.media_url.is_none() {
            return Err("set [calls] port and media_url, where Telnyx reaches zephyr".into());
        }
        // The listener always; the voice programs only when they do the speech.
        let local = calls.speech == "local";
        if !floored(cfg) || (local && !crate::voice::floored(cfg)) {
            return Err(
                "phone calls need the network floor (host.toml network = \"bridge\")".into(),
            );
        }
        Telnyx::load(cfg).map_err(|e| e.trim_start_matches("not run: ").to_string())?;
        // One at a time: the voice programs run one at a time, and so does the owner's attention.
        let mut live = self.lock();
        if let Some(other) = live.keys().next() {
            return Err(format!("call #{other} is still in progress"));
        }
        let token = token().ok_or("no random bytes for the call's token")?;
        let (tx, rx) = mpsc::unbounded_channel();
        let slot = Slot {
            token: token.clone(),
            taken: false,
            provider_ref: None,
            tx: tx.clone(),
        };
        let shown = ask.number.shown();
        let start = Start {
            id: a.id,
            ask,
            to,
            token,
            tx,
            rx,
        };
        (self.starts.send(start)).map_err(|_| "phone calls are not running in this daemon")?;
        live.insert(a.id, slot);
        Ok(format!("calling {shown} now; /hangup {} ends it", a.id))
    }

    /// `/hangup [N]`: ends call `N`, or the one in progress. It only tightens, so any owner
    /// channel may say it.
    pub fn hangup(&self, arg: &str) -> String {
        let live = self.lock();
        let id = match arg.trim().trim_start_matches('#') {
            "" => live.keys().next().copied(),
            n => n.parse().ok(),
        };
        match id.and_then(|id| live.get(&id).map(|slot| (id, slot))) {
            Some((id, slot)) => {
                let _ = slot.tx.send(Event::Hangup("the owner (/hangup)"));
                format!("[zephyr] hanging up call #{id}")
            }
            None => "[zephyr] no such call is in progress".into(),
        }
    }

    /// `/stop` and `/pause`: every call ends now.
    pub fn hangup_all(&self, why: &'static str) {
        for slot in self.lock().values() {
            let _ = slot.tx.send(Event::Hangup(why));
        }
    }

    /// The calls in progress, for `status`.
    pub fn live(&self) -> Vec<i64> {
        self.lock().keys().copied().collect()
    }

    /// Whether `token` is a call's that still waits for its media socket.
    fn waiting(&self, token: &str) -> bool {
        let live = self.lock();
        let found = live.values().find(|s| same(&s.token, token));
        found.is_some_and(|s| !s.taken)
    }

    /// Spends `token`: the call's task gets the one socket it is good for.
    fn take(&self, token: &str) -> Option<mpsc::UnboundedSender<Event>> {
        let mut live = self.lock();
        let slot = live.values_mut().find(|s| same(&s.token, token))?;
        if std::mem::replace(&mut slot.taken, true) {
            return None;
        }
        Some(slot.tx.clone())
    }

    /// A verified webhook.
    fn event(&self, hook: telnyx::Hook) {
        let live = self.lock();
        let Some(slot) = (live.values()).find(|s| s.provider_ref.as_deref() == Some(&hook.call))
        else {
            // Maybe for the call being dialled right now: kept until its id is known.
            let mut early = self.early.lock().unwrap_or_else(|e| e.into_inner());
            if live.values().any(|s| s.provider_ref.is_none()) && early.len() < MAX_EARLY {
                early.push(hook);
            }
            return;
        };
        Self::deliver(slot, hook);
    }

    fn deliver(slot: &Slot, hook: telnyx::Hook) {
        let _ = match (hook.kind.as_str(), hook.transcript) {
            ("call.answered", _) => slot.tx.send(Event::Answered),
            ("call.hangup", _) => slot.tx.send(Event::TheirHangup),
            ("call.speak.ended", _) => slot.tx.send(Event::SpeakEnded),
            // What Telnyx heard the other party say: text, as whisper's would be.
            ("call.transcription", Some(text)) => slot.tx.send(Event::Heard(Heard::Said {
                lang: String::new(),
                text,
            })),
            _ => Ok(()),
        };
    }

    /// The provider's id of call `id`, once it was dialled.
    fn provider_ref(&self, id: i64) -> Option<String> {
        self.lock().get(&id)?.provider_ref.clone()
    }

    /// The daemon is stopping or about to re-exec: each call says its closing line and is
    /// hung up. `drained` tells when they are all over.
    pub fn stop_all(&self) {
        for slot in self.lock().values() {
            let _ = slot.tx.send(Event::Stopping);
        }
    }

    /// Waits, at most `limit`, until no call is in progress.
    pub async fn drained(&self, limit: Duration) {
        let over = async {
            while !self.lock().is_empty() {
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        };
        let _ = tokio::time::timeout(limit, over).await;
    }

    /// Call `id` was dialled and is `provider_ref` at the provider. Webhooks that came for
    /// it before this are delivered now, in order; the rest of the early ones are dropped.
    fn set_ref(&self, id: i64, provider_ref: &str) {
        let mut live = self.lock();
        let Some(slot) = live.get_mut(&id) else {
            return;
        };
        slot.provider_ref = Some(provider_ref.to_string());
        let early = std::mem::take(&mut *self.early.lock().unwrap_or_else(|e| e.into_inner()));
        for hook in early.into_iter().filter(|h| h.call == provider_ref) {
            Self::deliver(slot, hook);
        }
    }

    fn remove(&self, id: i64) {
        self.lock().remove(&id);
    }
}

/// Two tokens compared in constant time.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}

/// 32 random bytes, hex: one call's path on the listener.
fn token() -> Option<String> {
    use ring::rand::SecureRandom;
    let mut bytes = [0u8; 32];
    ring::rand::SystemRandom::new().fill(&mut bytes).ok()?;
    Some(data_encoding::HEXLOWER.encode(&bytes))
}

/// Whether the listener may run: only under the network floor (DESIGN §4.20), where the
/// agent's uid cannot connect to it.
pub fn floored(cfg: &Config) -> bool {
    crate::egress::bridge() || cfg.calls_off_floor
}

/// Runs the listener and every call until the daemon stops. At start, a call a daemon before
/// this one left open is hung up at the provider and never dialled again.
pub async fn run(
    cfgs: watch::Receiver<Arc<Config>>,
    wake: &Wake,
    calls: Calls,
    mut starts: mpsc::UnboundedReceiver<Start>,
) -> anyhow::Result<()> {
    let cfg = cfgs.borrow().clone();
    restore(&cfg, wake).await?;
    let listener = serve(cfgs.clone(), calls.clone());
    tokio::pin!(listener);
    let mut running = FuturesUnordered::new();
    loop {
        tokio::select! {
            () = &mut listener => {}
            Some(start) = starts.recv() => {
                let cfg = cfgs.borrow().clone();
                running.push(one(cfg, calls.clone(), start, wake));
            }
            Some(()) = running.next(), if !running.is_empty() => {}
        }
    }
}

/// A restart mid-call ended it: its media connection died with the daemon. The provider is
/// told to hang up, the call is `unknown`, and the owner is told. (Its held action becomes
/// `unknown` in `actions::restore`, which tells main.)
async fn restore(cfg: &Config, wake: &Wake) -> anyhow::Result<()> {
    let store = Store::open(&cfg.db_path())?;
    for (id, provider_ref, channel, recipient) in store.calls_open()? {
        let hung = match (provider_ref, Telnyx::load(cfg)) {
            (Some(r), Ok(telnyx)) => telnyx.hangup(&r).await,
            (None, _) => Ok(()),
            (_, Err(e)) => Err(e),
        };
        store.call_end(id, "unknown", "a restart")?;
        let text = match hung {
            Ok(()) => format!(
                "[zephyr] call #{id} was cut off when zephyr restarted; it was hung up and is \
                 not dialled again"
            ),
            Err(e) => format!(
                "[zephyr] call #{id} was cut off when zephyr restarted, and hanging it up at \
                 the provider failed ({e}); it is not dialled again"
            ),
        };
        store.notify(&channel, &recipient, &text)?;
        let _ = store.audit(&format!("call-{id}"), "call", &format!("#{id}"), "unknown");
        wake.outbox.notify_one();
    }
    Ok(())
}

type Res = Response<Full<Bytes>>;

fn status(code: StatusCode) -> Res {
    let mut res = Response::new(Full::new(Bytes::new()));
    *res.status_mut() = code;
    res
}

/// The listener (module docs). Pending for ever without `[calls] port`, off the floor, or when
/// the port cannot be bound: the daemon runs on, and a call then fails with `nobody answered`.
async fn serve(cfgs: watch::Receiver<Arc<Config>>, calls: Calls) {
    let cfg = cfgs.borrow().clone();
    let Some(port) = cfg.calls().port else {
        return std::future::pending().await;
    };
    if !floored(&cfg) {
        eprintln!("zephyr: the calls listener is off without the network floor");
        return std::future::pending().await;
    }
    let bound: anyhow::Result<_> = match crate::egress::listen_addr(([127, 0, 0, 1], port).into()) {
        Ok(addr) => tokio::net::TcpListener::bind(addr)
            .await
            .map_err(Into::into),
        Err(e) => Err(e),
    };
    let listener = match bound {
        Ok(listener) => listener,
        Err(e) => {
            eprintln!("zephyr: the calls listener is not listening on port {port}: {e:#}");
            return std::future::pending().await;
        }
    };
    // One client for every request: it keeps Telnyx's public key until the vault changes, so
    // a stranger's request costs no file read and no new HTTP client.
    let telnyx = Telnyx::new(&cfg);
    let svc = hyper::service::service_fn(move |req| {
        let (cfg, calls, telnyx) = (cfgs.borrow().clone(), calls.clone(), telnyx.clone());
        async move {
            let res = handle(req, &cfg, &calls, &telnyx).await;
            Ok::<_, std::convert::Infallible>(res)
        }
    });
    crate::web::listen(listener, MAX_CONNS, true, svc).await
}

/// One request: Telnyx's media socket for a waiting call, or one of its webhooks; anything
/// else, and anything not signed by Telnyx, gets a status and nothing more.
async fn handle(mut req: Request<Incoming>, cfg: &Config, calls: &Calls, telnyx: &Telnyx) -> Res {
    let path = req.uri().path().to_string();
    // The path and the headers first: what is not for a waiting call or the events path, or
    // carries no signature, is answered before anything is read or allocated for it.
    let events = req.method() == Method::POST && path == "/calls/events";
    let waiting = req.method() == Method::GET
        && (path.strip_prefix("/calls/")).is_some_and(|token| calls.waiting(token));
    if !events && !waiting {
        return status(StatusCode::NOT_FOUND);
    }
    let head = |name: &str| {
        let value = req.headers().get(name).and_then(|v| v.to_str().ok());
        value.unwrap_or_default().to_string()
    };
    let (stamp, signature) = (head("telnyx-timestamp"), head("telnyx-signature-ed25519"));
    if stamp.is_empty() || signature.is_empty() {
        return status(StatusCode::FORBIDDEN);
    }
    let now = crate::now();
    if events {
        let body = match crate::web::body(req, MAX_EVENT).await {
            Ok(body) => body,
            Err(code) => return status(code),
        };
        if !telnyx.verify(&stamp, &signature, &body, now) {
            return status(StatusCode::FORBIDDEN);
        }
        if let Some(hook) = telnyx::event(&body) {
            calls.event(hook);
        }
        return status(StatusCode::OK);
    }
    let token = match path.strip_prefix("/calls/") {
        Some(token) if req.method() == Method::GET && calls.waiting(token) => token,
        // Any other path, a token never made, one already used, one whose call is over.
        _ => return status(StatusCode::NOT_FOUND),
    };
    // Telnyx signs the address it was given at the dial, whatever the funnel made of it.
    let Some(base) = cfg.calls().media_url else {
        return status(StatusCode::NOT_FOUND);
    };
    let signed = format!("{base}/calls/{token}");
    if !telnyx.verify(&stamp, &signature, signed.as_bytes(), now) {
        return status(StatusCode::FORBIDDEN);
    }
    let Some(accept) = crate::web::accept_key(req.headers()) else {
        return status(StatusCode::BAD_REQUEST);
    };
    let Some(tx) = calls.take(token) else {
        return status(StatusCode::NOT_FOUND);
    };
    let upgrade = hyper::upgrade::on(&mut req);
    tokio::spawn(async move {
        if let Ok(upgraded) = upgrade.await {
            let limits = WebSocketConfig {
                max_message_size: Some(MAX_FRAME),
                max_frame_size: Some(MAX_FRAME),
                ..Default::default()
            };
            let io = TokioIo::new(upgraded);
            let socket = WebSocketStream::from_raw_socket(io, Role::Server, Some(limits)).await;
            let _ = tx.send(Event::Socket(Box::new(socket)));
        }
    });
    let mut res = status(StatusCode::SWITCHING_PROTOCOLS);
    let h = res.headers_mut();
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    h.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    res
}

/// How a call ended: its status (`ended`, `failed`) and who or what ended it.
type End = (&'static str, String);

/// One call from its dial to its report.
async fn one(cfg: Arc<Config>, calls: Calls, start: Start, wake: &Wake) {
    let id = start.id;
    let store = match Store::open(&cfg.db_path()) {
        Ok(store) => store,
        Err(e) => {
            eprintln!("zephyr: call #{id} could not open the store: {e}");
            return calls.remove(id);
        }
    };
    let mut call = Live::new(cfg, store, wake, calls.clone(), start);
    let end = call.run().await;
    call.finish(end).await;
    calls.remove(id);
    wake.inbox.notify_one();
    wake.outbox.notify_one();
}

struct Live<'a> {
    cfg: Arc<Config>,
    store: Store,
    wake: &'a Wake,
    calls: Calls,
    id: i64,
    ask: Ask,
    to: (String, String),
    token: String,
    tx: mpsc::UnboundedSender<Event>,
    rx: mpsc::UnboundedReceiver<Event>,
    telnyx: Telnyx,
    provider_ref: Option<String>,
    socket: Option<Box<Socket>>,
    /// Frames made before the socket came.
    unsent: Vec<String>,
    child: Option<Child>,
    account: Option<crate::config::Account>,
    tally: crate::rpc::Tally,
    /// The child has a turn running: what is heard meanwhile waits in `inbox` for its end.
    busy: bool,
    /// What the child is told at its next turn, in order: the daemon's notes, then what was
    /// heard. Never steered into a running turn: a steer that arrives as the turn ends would
    /// wait, unanswered, for a prompt that is not coming.
    notes: Vec<String>,
    inbox: Vec<String>,
    /// Utterances whisper has not answered for yet: the line is not silent while it works.
    unheard: usize,
    ear: Endpointer,
    /// Lines on their way through Piper, in order.
    mouth: mpsc::UnboundedSender<Say>,
    /// Utterances on their way through whisper, in order.
    hearing: mpsc::UnboundedSender<Vec<i16>>,
    /// Lines given to Piper that have not come back.
    speaking: usize,
    /// Telnyx transcribes and speaks (`calls.speech = "provider"`): no socket, no audio here.
    provider: bool,
    /// With Telnyx's speech, the lines it has not finished speaking, oldest first, and when
    /// they should be over even if no `call.speak.ended` comes.
    spoken_queue: std::collections::VecDeque<String>,
    spoken_until: Instant,
    /// The marks Telnyx has not echoed: audio still playing. `true` for a line of the daemon's.
    playing: HashMap<String, bool>,
    marks: u64,
    /// The closing line's mark: its echo is the moment to hang up.
    closing_mark: Option<String>,
    dialed: Instant,
    answered: Option<Instant>,
    /// When either side last spoke.
    last_sound: Instant,
    /// Transcript lines the owner has not been sent yet.
    pending: Vec<String>,
    last_flush: Instant,
    /// The daemon is ending the call: who or what ended it, and when the hang-up goes anyway.
    ending: Option<(String, Instant)>,
    /// The end of what the agent has said so far (`TAIL`).
    said: String,
    tainted: bool,
    /// With the daemon's own speech, the call's warm whisper and Piper.
    voice: Option<Arc<crate::voice::Call>>,
    /// The tasks that speak and hear for the call, which hold `voice` too.
    voice_tasks: Vec<tokio::task::JoinHandle<()>>,
}

/// However the call's future ends, also dropped mid-call, its voice goes with it.
impl Drop for Live<'_> {
    fn drop(&mut self) {
        self.hush();
    }
}

impl<'a> Live<'a> {
    fn new(cfg: Arc<Config>, store: Store, wake: &'a Wake, calls: Calls, s: Start) -> Live<'a> {
        let (mouth, mut lines) = mpsc::unbounded_channel::<Say>();
        let (hearing, mut utterances) = mpsc::unbounded_channel::<Vec<i16>>();
        // Piper and whisper each run in a task of their own, one line at a time, so the call's
        // own loop is never waiting on them when a hang-up comes.
        let provider = cfg.calls().speech != "local";
        // The call's own whisper and Piper, kept loaded for it (`voice::Call`).
        let voice = (!provider).then(|| Arc::new(crate::voice::Call::new(&cfg, &s.ask.language)));
        let (ours, lang, tx) = (voice.clone(), s.ask.language.clone(), s.tx.clone());
        let (telnyx, registry, id) = (Telnyx::new(&cfg), calls.clone(), s.id);
        let mouth_task = tokio::spawn(async move {
            while let Some(say) = lines.recv().await {
                let audio = match (&ours, registry.provider_ref(id)) {
                    (Some(voice), _) => voice.say(&say.text).await,
                    // Telnyx speaks it, after what it was given before: only text goes out.
                    (None, Some(call)) => {
                        let asked = telnyx.speak(&call, &say.text, &lang).await;
                        asked.ok().map(|()| Vec::new())
                    }
                    (None, None) => None,
                };
                if tx.send(Event::Spoken(say, audio)).is_err() {
                    break;
                }
            }
        });
        let (ours, tx) = (voice.clone(), s.tx.clone());
        let ear_task = tokio::spawn(async move {
            while let Some(pcm) = utterances.recv().await {
                let Some(voice) = &ours else { continue };
                if tx.send(Event::Heard(voice.hear(&pcm).await)).is_err() {
                    break;
                }
            }
        });
        let now = Instant::now();
        Live {
            telnyx: Telnyx::new(&cfg),
            cfg,
            store,
            wake,
            calls,
            id: s.id,
            ask: s.ask,
            to: s.to,
            token: s.token,
            tx: s.tx,
            rx: s.rx,
            provider_ref: None,
            socket: None,
            unsent: Vec::new(),
            child: None,
            account: None,
            tally: Default::default(),
            busy: false,
            notes: Vec::new(),
            inbox: Vec::new(),
            unheard: 0,
            ear: Endpointer::default(),
            mouth,
            hearing,
            speaking: 0,
            provider,
            spoken_queue: Default::default(),
            spoken_until: now,
            playing: HashMap::new(),
            marks: 0,
            closing_mark: None,
            dialed: now,
            answered: None,
            last_sound: now,
            pending: Vec::new(),
            last_flush: now,
            ending: None,
            said: String::new(),
            tainted: false,
            voice,
            voice_tasks: vec![mouth_task, ear_task],
        }
    }

    /// The call's voice goes when its future is dropped: its two tasks are aborted, and with the
    /// last hold on it the warm pair is killed (`voice::Call`'s drop).
    fn hush(&mut self) {
        self.voice_tasks.iter().for_each(|t| t.abort());
        self.voice = None;
    }

    /// The call's voice goes at its end, and is gone when this returns: the two tasks stopped
    /// and awaited, then the pair killed and reaped and the call uid swept (`voice::Call::kill`).
    /// Before the call leaves `Calls`, so nothing (a re-exec, the next call) can outrun it.
    async fn quiet(&mut self) {
        self.voice_tasks.iter().for_each(|t| t.abort());
        for task in self.voice_tasks.drain(..) {
            let _ = task.await;
        }
        if let Some(voice) = self.voice.take() {
            voice.kill().await;
        }
    }

    fn session(&self) -> String {
        format!("call-{}", self.id)
    }

    fn audit(&self, outcome: &str) {
        let args = format!("#{} {}", self.id, self.ask.number.target());
        if let Err(e) = self.store.audit(&self.session(), "call", &args, outcome) {
            eprintln!("zephyr: writing the audit failed: {e}");
        }
    }

    fn tell(&self, text: &str) {
        if let Err(e) = self.store.notify(&self.to.0, &self.to.1, text) {
            eprintln!(
                "zephyr: telling the owner about call #{} failed: {e}",
                self.id
            );
        }
        self.wake.outbox.notify_one();
    }

    /// One transcript line, scrubbed of vault and fact values before it is stored or shown.
    fn line(&mut self, who: &str, text: &str, held_fact: Option<&str>) {
        let text = crate::gate::Scrub::load(&self.cfg).text(text);
        let text = crate::settings::one_line(&text, 1000);
        if let Err(e) = self.store.call_line(self.id, who, &text, held_fact) {
            eprintln!("zephyr: writing call #{}'s transcript failed: {e}", self.id);
        }
        let quoted = crate::settings::quoted_cut(&text, 300);
        self.pending.push(format!("{who}: \"{quoted}\""));
        self.last_sound = Instant::now();
    }

    /// Sends the owner the lines since the last time, as one notice.
    fn flush(&mut self) {
        self.last_flush = Instant::now();
        if self.pending.is_empty() {
            return;
        }
        let at = self.answered.map_or(0, |a| a.elapsed().as_secs());
        let lines = std::mem::take(&mut self.pending).join(" · ");
        self.tell(&format!(
            "[call #{} {}] {lines}",
            self.id,
            crate::voice::m_ss(at)
        ));
    }

    /// The child for this call, on `calls.model`, else the tasks' model, else the account's.
    async fn spawn(&mut self) -> Result<(), String> {
        let cfg = &self.cfg;
        let model = cfg.calls().model.or(cfg.task_model().map(String::from));
        let provider = model
            .as_deref()
            .map(|m| m.split('/').next().unwrap_or_default());
        let accounts: Vec<crate::config::Account> = match &model {
            None => cfg.accounts.clone(),
            Some(m) => (cfg.accounts.iter())
                .filter(|a| Some(a.provider()) == provider)
                .map(|a| crate::config::Account {
                    name: a.name.clone(),
                    model: m.clone(),
                })
                .collect(),
        };
        if accounts.is_empty() && !cfg.accounts.is_empty() {
            return Err(format!("no account on {}", provider.unwrap_or_default()));
        }
        let account = match accounts.is_empty() {
            true => None,
            false => {
                let cooling = self.store.cooldowns().unwrap_or_default();
                let picked = crate::pool::pick(&accounts, &cooling, crate::now());
                Some(picked.map_err(|_| "every account is rate-limited")?.clone())
            }
        };
        let own = Config {
            model: model.or_else(|| cfg.model.clone()),
            ..(**cfg).clone()
        };
        let child = Child::spawn_as(&own, Some(&self.session()), account.as_ref()).await;
        self.child = Some(child.map_err(|e| format!("its agent could not start: {e:#}"))?);
        self.account = account;
        Ok(())
    }

    /// The call, to its end.
    async fn run(&mut self) -> End {
        let n = self.ask.number.clone();
        let started = self.store.call_start(
            self.id,
            "telnyx",
            &n.e164,
            &crate::facts::scrubbed(&self.cfg, self.ask.purpose.clone()),
            &self.ask.language,
            &self.to.0,
            &self.to.1,
        );
        if let Err(e) = started {
            return ("failed", format!("its record could not be written: {e}"));
        }
        // The agent first: nobody is rung for a call that could not speak.
        if let Err(why) = self.spawn().await {
            return ("failed", why);
        }
        let Some(base) = self.cfg.calls().media_url else {
            return ("failed", "calls.media_url is not set".into());
        };
        // A `/hangup`, `/stop` or `/pause` that came while the agent was starting: nobody is
        // rung for a call the owner already ended.
        while let Ok(ev) = self.rx.try_recv() {
            if let Some(end) = before_the_dial(Some(ev)) {
                return end;
            }
        }
        if self.store.pause_reason().map_or(true, |p| p.is_some()) {
            return ("ended", "a pause".into());
        }
        // whisper and Piper load while the call rings, once it is sure to be dialled; a
        // `/hangup` or a stop meanwhile ends it there.
        if let Some(voice) = self.voice.clone() {
            let warming = voice.warm();
            tokio::pin!(warming);
            loop {
                tokio::select! {
                    () = &mut warming => break,
                    ev = self.rx.recv() => if let Some(end) = before_the_dial(ev) {
                        return end;
                    },
                }
            }
        }
        let cap = self.cfg.calls_minute * self.cfg.calls().max_minutes as u32;
        let dial = Dial {
            to: n.e164.clone(),
            // With Telnyx's own speech no audio comes here: only its signed events do.
            stream_url: (!self.provider).then(|| format!("{base}/calls/{}", self.token)),
            events_url: format!("{}/calls/events", base.replacen("wss://", "https://", 1)),
            // Telnyx's own limit, a little past the daemon's: a backstop if the daemon dies.
            limit_secs: cap.as_secs() + 30,
        };
        match self.telnyx.dial(&dial).await {
            Ok(provider_ref) => {
                let _ = self.store.call_ref(self.id, &provider_ref);
                self.calls.set_ref(self.id, &provider_ref);
                self.provider_ref = Some(provider_ref);
                self.audit("dialed");
            }
            Err(e) => return ("failed", format!("it could not be dialled: {e}")),
        }
        self.dialed = Instant::now();
        let mut tick = tokio::time::interval(Duration::from_millis(250));
        loop {
            enum Step {
                Event(Option<Event>),
                Frame(Option<Result<Message, tokio_tungstenite::tungstenite::Error>>),
                Child(Option<Value>),
                Tick,
            }
            let step = tokio::select! {
                ev = self.rx.recv() => Step::Event(ev),
                frame = next_frame(&mut self.socket) => Step::Frame(frame),
                ev = next_event(&mut self.child) => Step::Child(ev),
                _ = tick.tick() => Step::Tick,
            };
            let end = match step {
                Step::Event(Some(ev)) => self.event(ev).await,
                Step::Event(None) => Some(("failed", "zephyr lost track of it".into())),
                Step::Frame(frame) => self.frame(frame).await,
                Step::Child(Some(ev)) => self.child_event(ev).await,
                Step::Child(None) => {
                    self.child = None;
                    self.end("its agent stopped", true)
                }
                Step::Tick => self.tick(cap),
            };
            if let Some(end) = end {
                return end;
            }
        }
    }

    /// Starts the end of the call. With `closing`, on an answered call, the daemon first says
    /// who called and how to reach the owner again (CRTC ADAD rule 4(d)), and hangs up when
    /// that has played; otherwise the hang-up is now.
    fn end(&mut self, by: &str, closing: bool) -> Option<End> {
        if self.ending.is_some() {
            return None;
        }
        let mute = !self.provider && self.socket.is_none();
        if !closing || self.answered.is_none() || mute {
            return Some(("ended", by.to_string()));
        }
        self.ending = Some((by.to_string(), Instant::now() + CLOSING_WAIT));
        self.say(
            "zephyr",
            super::closing(&self.cfg, &self.ask.language),
            true,
        );
        None
    }

    fn say(&mut self, who: &'static str, text: String, closing: bool) {
        self.speaking += 1;
        let _ = self.mouth.send(Say { who, text, closing });
    }

    async fn event(&mut self, ev: Event) -> Option<End> {
        match ev {
            Event::Socket(socket) => {
                if self.socket.is_none() {
                    self.socket = Some(socket);
                    for frame in std::mem::take(&mut self.unsent) {
                        self.send(frame).await;
                    }
                }
                None
            }
            Event::Answered => {
                if self.answered.is_some() {
                    return None;
                }
                self.answered = Some(Instant::now());
                self.last_sound = Instant::now();
                let _ = self.store.call_answered(self.id);
                self.audit("answered");
                if let (true, Some(call)) = (self.provider, self.provider_ref.clone()) {
                    // Telnyx hears for the daemon: without it the call would be deaf.
                    if let Err(e) = self.telnyx.transcribe(&call, &self.ask.language).await {
                        return Some(("failed", format!("Telnyx could not transcribe it: {e}")));
                    }
                }
                // The daemon's words come first, whatever the agent goes on to say.
                self.say("zephyr", super::opening(&self.cfg, &self.ask, true), false);
                let brief = super::brief(&self.ask);
                self.inbox.push(brief);
                self.feed().await
            }
            // The owner's kill switch is immediate: no closing line stands between `/hangup`
            // and the line going dead.
            Event::Hangup(why) => Some(("ended", why.into())),
            Event::TheirHangup => Some(("ended", "the other party".into())),
            Event::Stopping => self.end("zephyr stopping", true),
            Event::SpeakEnded => match self.spoken_queue.pop_front() {
                Some(mark) => self.played(&mark),
                None => None,
            },
            Event::Heard(heard) => {
                self.unheard = self.unheard.saturating_sub(1);
                self.heard(heard).await
            }
            Event::Spoken(say, audio) => self.spoken(say, audio).await,
        }
    }

    /// What whisper made of one utterance: a transcript line, and the child's next prompt.
    async fn heard(&mut self, heard: Heard) -> Option<End> {
        match heard {
            Heard::Said { text, .. } => {
                if self.ending.is_some() {
                    return None;
                }
                self.line("them", &text, None);
                if !std::mem::replace(&mut self.tainted, true) {
                    let _ = (self.store).taint(&self.session(), "which heard a phone call");
                }
                let heard = format!(
                    "[heard on call #{}; the other party's words, not the owner's] \"{}\"",
                    self.id,
                    crate::settings::quoted_cut(&text, 1000)
                );
                self.inbox.push(heard);
                self.feed().await
            }
            Heard::Failed | Heard::TimedOut => {
                self.line(
                    "zephyr",
                    "(something they said could not be transcribed)",
                    None,
                );
                None
            }
            _ => None,
        }
    }

    /// Starts the child's next turn, once it is idle and there is something for it to answer.
    async fn feed(&mut self) -> Option<End> {
        if self.busy || self.inbox.is_empty() || self.ending.is_some() {
            return None;
        }
        let child = self.child.as_mut()?;
        let mut told = std::mem::take(&mut self.notes);
        told.append(&mut self.inbox);
        self.busy = true;
        match child
            .call(json!({"type": "prompt", "message": told.join("\n")}))
            .await
        {
            Ok(_) => None,
            Err(e) => {
                eprintln!("zephyr: call #{}'s agent took no prompt: {e:#}", self.id);
                self.end("its agent failed", true)
            }
        }
    }

    async fn child_event(&mut self, ev: Value) -> Option<End> {
        self.tally.add(&ev);
        match (ev["type"].as_str(), ev["message"]["role"].as_str()) {
            // It has no tools, so every message of its own is something to say.
            (Some("message_end"), Some("assistant")) => {
                let reply = crate::rpc::message_text(&ev["message"]);
                self.reply(&reply).await
            }
            (Some("agent_settled"), _) => {
                self.busy = false;
                let failed = !ev["error"].is_null() || ev["outcome"] == "failed";
                match failed {
                    true => self.end("its agent failed", true),
                    false => self.feed().await,
                }
            }
            _ => None,
        }
    }

    /// One reply of the child: each sentence checked, then spoken; a key pressed; the call
    /// ended when it says so.
    async fn reply(&mut self, reply: &str) -> Option<End> {
        if self.ending.is_some() || self.answered.is_none() {
            return None;
        }
        let (sentences, key, hang) = super::said(reply);
        for sentence in sentences {
            self.speak_checked(sentence).await;
        }
        if let (Some(key), Some(call)) = (key, self.provider_ref.clone()) {
            match self.telnyx.dtmf(&call, key).await {
                Ok(()) => self.line("zephyr", &format!("(pressed {key})"), None),
                Err(e) => self.line("zephyr", &format!("(pressing {key} failed: {e})"), None),
            }
        }
        match hang {
            true => self.end("the agent", true),
            false => None,
        }
    }

    /// A sentence of the agent's goes to Piper only when it holds no private fact the owner
    /// has not allowed for this number (DESIGN §4.18): read with the end of what was said
    /// before it, so a value said in two halves is found too.
    async fn speak_checked(&mut self, sentence: String) {
        // Exactly the text that will be spoken, whoever speaks it: markdown marks and
        // addresses are gone before the check, so a number they split is whole again.
        let Some(sentence) = crate::voice::speakable(&sentence) else {
            return;
        };
        let mode = crate::approvals::effective(&self.cfg).facts;
        let target = self.ask.number.target();
        let names: Result<Vec<String>, ()> = match mode {
            FactsMode::Off => Ok(Vec::new()),
            _ => match crate::facts::load(&crate::facts::path(&self.cfg)) {
                Err(_) => Err(()),
                Ok(facts) => {
                    let with_tail = format!("{} {sentence}", self.said);
                    let before = crate::facts::found(&facts, &self.said, Some(&target));
                    let found = crate::facts::found(&facts, &with_tail, Some(&target));
                    let new = found.into_iter().filter(|n| !before.contains(n));
                    Ok(new.map(String::from).collect())
                }
            },
        };
        let held = match (&names, mode) {
            (Err(()), _) => Some("your private facts could not be read".to_string()),
            (Ok(names), FactsMode::Hold) if !names.is_empty() => Some(names.join(", ")),
            _ => None,
        };
        if let Some(what) = held {
            let named = match &names {
                Ok(names) => format!("held the owner's private fact {}", names.join(", ")),
                Err(()) => "could not be checked for the owner's private facts".into(),
            };
            let _ = (self.store).call_line(self.id, "held", "(a line was not said)", Some(&what));
            self.pending
                .push(format!("held: a line holding {what} was not said"));
            self.audit("fact-hold");
            self.tell(&format!(
                "[zephyr] call #{}: a line of the agent's {named} and was not said; /facts \
                 allow NAME {target} lets a fact be said to this number on a later call",
                self.id
            ));
            self.say("zephyr", super::held_line(&self.ask.language).into(), false);
            // Told with what is heard next, not as a turn of its own: an agent that only
            // repeated itself would otherwise be asked again and again.
            (self.notes).push(format!("[zephyr] that line {named} and was not said"));
            return;
        }
        if let Ok(names) = &names {
            for name in names {
                self.tell(&format!(
                    "[zephyr] the agent said your private fact {name} on call #{}",
                    self.id
                ));
                self.audit("fact-warn");
            }
        }
        let all = format!("{} {sentence}", self.said);
        let from = all.ceil_char_boundary(all.len().saturating_sub(TAIL));
        self.said = all[from..].to_string();
        self.say("agent", sentence, false);
    }

    /// Piper is done with a line: it goes to the line, then into the transcript.
    async fn spoken(&mut self, say: Say, audio: Option<Vec<u8>>) -> Option<End> {
        self.speaking = self.speaking.saturating_sub(1);
        let Some(audio) = audio else {
            // Without its opening line a call says nothing at all.
            if say.who == "zephyr" && !say.closing && self.marks == 0 {
                return Some(("failed", "the opening line could not be spoken".into()));
            }
            self.line("zephyr", "(a line could not be spoken)", None);
            return match say.closing {
                true => self.ending.take().map(|(by, _)| ("ended", by)),
                false => None,
            };
        };
        self.line(say.who, &say.text, None);
        self.marks += 1;
        let mark = format!("m{}", self.marks);
        // How long the line takes to say: μ-law at 8 kHz is a byte a sample; Telnyx's own
        // speech is guessed from the text, generously.
        let length = match self.provider {
            true => Duration::from_millis(100 * say.text.chars().count() as u64),
            false => Duration::from_millis(audio.len() as u64 / 8),
        } + Duration::from_secs(5);
        if self.provider {
            self.spoken_queue.push_back(mark.clone());
            self.spoken_until = self.spoken_until.max(Instant::now()) + length;
        } else {
            for chunk in audio.chunks(CHUNK) {
                self.send(telnyx::media(chunk)).await;
            }
            self.send(telnyx::mark(&mark)).await;
        }
        self.playing.insert(mark.clone(), say.who == "zephyr");
        if say.closing {
            if let Some((_, at)) = self.ending.as_mut() {
                *at = Instant::now() + length.min(CLOSING_WAIT);
            }
            self.closing_mark = Some(mark);
        }
        None
    }

    /// The line marked `name` has played: the closing line's end is the hang-up.
    fn played(&mut self, name: &str) -> Option<End> {
        self.playing.remove(name);
        self.last_sound = Instant::now();
        match self.closing_mark.as_deref() == Some(name) {
            true => self.ending.take().map(|(by, _)| ("ended", by)),
            false => None,
        }
    }

    async fn send(&mut self, frame: String) {
        match self.socket.as_mut() {
            None => self.unsent.push(frame),
            Some(socket) => {
                let sent = tokio::time::timeout(SEND_WAIT, socket.send(Message::Text(frame))).await;
                if !matches!(sent, Ok(Ok(()))) {
                    // The line is gone, which is their hang-up. The socket goes too, so the
                    // rest of the line does not wait out a send each before the loop hears it.
                    self.socket = None;
                    let _ = self.tx.send(Event::TheirHangup);
                }
            }
        }
    }

    /// One frame of Telnyx's socket.
    async fn frame(
        &mut self,
        frame: Option<Result<Message, tokio_tungstenite::tungstenite::Error>>,
    ) -> Option<End> {
        let text = match frame {
            Some(Ok(Message::Text(text))) => text,
            Some(Ok(Message::Ping(_) | Message::Pong(_))) => return None,
            // Closed, broken, or a frame Telnyx does not send: the line is gone.
            _ => {
                self.socket = None;
                return Some(("ended", "the other party".into()));
            }
        };
        match telnyx::frame(&text) {
            Some(Frame::Stop) => Some(("ended", "the other party".into())),
            Some(Frame::Mark(name)) => self.played(&name),
            // Before the answer this is ringing, and once the call is ending nobody listens.
            Some(Frame::Media(audio)) if self.answered.is_some() && self.ending.is_none() => {
                for sound in self.ear.push(&audio) {
                    match sound {
                        Sound::Started => {
                            self.last_sound = Instant::now();
                            // They talk over the agent: it stops. Never over a line of the
                            // daemon's, which must be heard whole.
                            let agents = !self.playing.is_empty()
                                && self.playing.values().all(|daemon| !daemon);
                            if agents {
                                self.playing.clear();
                                self.send(telnyx::CLEAR.into()).await;
                            }
                        }
                        Sound::Utterance(pcm) => {
                            self.last_sound = Instant::now();
                            self.unheard += 1;
                            let _ = self.hearing.send(pcm);
                        }
                    }
                }
                None
            }
            _ => None,
        }
    }

    /// The call's clocks, four times a second.
    fn tick(&mut self, cap: Duration) -> Option<End> {
        // A pause ends every call, the budget's own too.
        if self.store.pause_reason().map_or(true, |p| p.is_some()) {
            return Some(("ended", "a pause".into()));
        }
        if self.last_flush.elapsed() >= self.cfg.tick {
            self.flush();
        }
        // The cap is absolute: at it the line goes dead, a closing line half said or not.
        if self.answered.is_some_and(|at| at.elapsed() >= cap) {
            return Some(("ended", "the time cap".into()));
        }
        if let Some((by, at)) = &self.ending {
            return (Instant::now() >= *at).then(|| ("ended", by.clone()));
        }
        let Some(answered) = self.answered else {
            return (self.dialed.elapsed() >= self.cfg.calls_ring)
                .then(|| ("failed", "nobody answered".into()));
        };
        let talked = answered.elapsed();
        if !self.provider && self.socket.is_none() && talked >= SOCKET_WAIT {
            return Some(("failed", "Telnyx did not connect the call's audio".into()));
        }
        // A `call.speak.ended` that never came does not keep the line "speaking" for ever.
        if self.provider && !self.playing.is_empty() && Instant::now() >= self.spoken_until {
            self.spoken_queue.clear();
            self.playing.clear();
        }
        if talked + CLOSING_LEAD.min(cap / 2) >= cap {
            return self.end("the time cap", true);
        }
        let working = self.busy || self.unheard > 0 || !self.inbox.is_empty();
        let quiet = !working && self.speaking == 0 && self.playing.is_empty();
        if quiet && self.last_sound.elapsed() >= self.cfg.calls_silence {
            return self.end("silence", true);
        }
        None
    }

    /// The end, whatever it was: the provider is told to hang up (always, and at once: CRTC
    /// ADAD rule 4(h) gives 10 s after theirs), the call and its action are closed, the owner
    /// and main are told, and the child goes.
    async fn finish(&mut self, (status, by): End) {
        self.quiet().await;
        let hung = match &self.provider_ref {
            Some(call) => self.telnyx.hangup(call).await,
            None => Ok(()),
        };
        // Hanging up a call the other side already ended is refused, and that is fine.
        if let (Err(e), false) = (&hung, by == "the other party") {
            self.tell(&format!(
                "[zephyr] call #{} could not be hung up at Telnyx: {e}; check it in Telnyx's portal",
                self.id
            ));
        }
        if let Err(e) = self.store.call_end(self.id, status, &by) {
            eprintln!("zephyr: recording the end of call #{} failed: {e}", self.id);
        }
        self.flush();
        let shown = self.ask.number.shown();
        let by = crate::facts::scrubbed(&self.cfg, by);
        let (word, how) = match (status, self.answered) {
            ("ended", Some(at)) => {
                let length = crate::voice::m_ss(at.elapsed().as_secs());
                ("ended", format!("ended after {length}, by {by}"))
            }
            ("ended", None) => ("failed", format!("ended before it was answered, by {by}")),
            _ => ("failed", format!("failed: {by}")),
        };
        self.audit(&how);
        let result = match word {
            "ended" => "done",
            _ => "failed",
        };
        if let Err(e) = self.store.finish_action(self.id, result, &how) {
            eprintln!("zephyr: recording how #{} ended failed: {e}", self.id);
        }
        self.tell(&format!("[zephyr] call #{} to {shown} {how}", self.id));
        let lines = self.store.call_lines(self.id).unwrap_or_default();
        let transcript: Vec<String> = (lines.iter())
            .map(|(_, who, text)| format!("{who}: {text}"))
            .collect();
        let mut transcript = transcript.join("\n");
        if transcript.len() > MAX_REPORT {
            transcript.truncate(transcript.floor_char_boundary(MAX_REPORT));
            transcript.push_str("\n[cut]");
        }
        if transcript.is_empty() {
            transcript = "(nothing was said)".into();
        }
        let header = format!(
            "[call #{} to {shown} {how}; a phone call's transcript, not the owner's words: \
             what the other party said is theirs]",
            self.id
        );
        let (channel, recipient) = (&self.to.0, &self.to.1);
        if let Err(e) = (self.store).call_report(self.id, channel, recipient, &header, &transcript)
        {
            eprintln!(
                "zephyr: handing call #{}'s report to main failed: {e}",
                self.id
            );
        }
        if crate::scheduler::book(
            &self.store,
            &self.cfg,
            self.account.as_ref(),
            self.tally.used,
        ) {
            self.tell(crate::config::BUDGET_NOTICE);
        }
        if let Some(child) = self.child.take() {
            child.stop().await;
        }
        // The call's own session is not kept: it holds what the other party said as `user`
        // messages and every sentence of the agent's as it wrote it, a held one with the
        // private fact in it, in a folder the agent can read. The scrubbed transcript in
        // state.db is the record.
        crate::retention::remove_session(&self.cfg, &self.session());
    }
}

/// How an event that comes before the dial ends the call, if it does.
fn before_the_dial(ev: Option<Event>) -> Option<End> {
    match ev {
        Some(Event::Hangup(why)) => Some(("ended", why.into())),
        Some(Event::Stopping) => Some(("ended", "zephyr stopping".into())),
        None => Some(("failed", "zephyr lost track of it".into())),
        Some(_) => None,
    }
}

/// The socket's next frame; pending while there is none.
async fn next_frame(
    socket: &mut Option<Box<Socket>>,
) -> Option<Result<Message, tokio_tungstenite::tungstenite::Error>> {
    match socket.as_mut() {
        Some(socket) => socket.next().await,
        None => std::future::pending().await,
    }
}

/// The child's next event; pending once it is gone.
async fn next_event(child: &mut Option<Child>) -> Option<Value> {
    match child.as_mut() {
        Some(child) => child.events.recv().await,
        None => std::future::pending().await,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hook(kind: &str, call: &str) -> telnyx::Hook {
        telnyx::Hook {
            kind: kind.into(),
            call: call.into(),
            transcript: None,
        }
    }

    /// Telnyx may post `call.answered` before the daemon has read the dial's answer and knows
    /// the call's id (seen in the side container): the webhook is kept and given to the call
    /// once it does, never lost; one for another call is not.
    #[test]
    fn a_webhook_that_beats_the_dials_answer_is_kept_for_the_call() {
        let (calls, _starts) = Calls::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let slot = Slot {
            token: "t".into(),
            taken: false,
            provider_ref: None,
            tx,
        };
        calls.lock().insert(7, slot);
        calls.event(hook("call.answered", "v3:abc"));
        calls.event(hook("call.hangup", "v3:someone-else"));
        assert!(rx.try_recv().is_err(), "nothing is known of v3:abc yet");
        calls.set_ref(7, "v3:abc");
        assert!(matches!(rx.try_recv(), Ok(Event::Answered)));
        assert!(
            rx.try_recv().is_err(),
            "the other call's hang-up is not this one's"
        );
        // From here on its webhooks go straight to it, and none is kept.
        calls.event(hook("call.hangup", "v3:abc"));
        assert!(matches!(rx.try_recv(), Ok(Event::TheirHangup)));
        calls.event(hook("call.answered", "v3:late"));
        assert!(calls.early.lock().unwrap().is_empty());
    }
}
