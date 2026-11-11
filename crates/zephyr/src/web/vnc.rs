//! Desktop takeover (DESIGN §4.3.3): the owner takes the agent's desktop from the web app.
//! `take` is passkey-signed (`attach`), answers a ticket, and `GET /api/vnc` joins a WebSocket
//! to an `x11vnc -inetd` whose only socket is one end of an unnamed socketpair: it listens
//! nowhere, so the agent has nothing to connect to. `watchdog` gives the desktop back by itself.

use super::{App, Res};
use crate::config::Config;
use crate::store::WebSession;
use futures_util::{SinkExt, StreamExt};
use hyper::body::Incoming;
use hyper::header::{self, HeaderValue};
use hyper::{Request, StatusCode};
use hyper_util::rt::TokioIo;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio_tungstenite::tungstenite::protocol::{Role, WebSocketConfig};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

/// How long a ticket opens `/api/vnc`.
const TICKET_LIFE: Duration = Duration::from_secs(30);
/// Tickets waiting at once; one more drops the oldest.
const MAX_TICKETS: usize = 16;
/// The largest frame either way.
const MAX_FRAME: usize = 1 << 20;
/// How long one write may take, to x11vnc or to a phone that stopped reading.
const WRITE_WAIT: Duration = Duration::from_secs(30);
/// The subprotocol the app offers beside its ticket (`zt.<ticket>`), and the one answered.
const PROTOCOL: &str = "zephyr-vnc";
/// x11vnc speaks on its stdin and listens nowhere (`-inetd`), serves that one client (`-once`),
/// reads no rc file of the agent's (`-norc`) and takes no command from an X client or a
/// connected viewer (`-nocmds`, `-noremote`).
const X11VNC_ARGS: [&str; 7] = [
    "-inetd",
    "-once",
    "-norc",
    "-nocmds",
    "-noremote",
    "-nossl",
    "-quiet",
];

/// What opens `/api/vnc` once: 32 random bytes, of which only the SHA-256 is kept, for 30 s,
/// bound to the session that asked.
#[derive(Default)]
pub struct Tickets(Mutex<Vec<Ticket>>);

/// A waiting ticket: its SHA-256, its session, and when it was made.
type Ticket = ([u8; 32], [u8; 32], Instant);

impl Tickets {
    /// A new ticket for `session`: base64url of 32 random bytes.
    pub fn issue(&self, session: [u8; 32]) -> String {
        self.issue_at(session, Instant::now())
    }

    fn issue_at(&self, session: [u8; 32], now: Instant) -> String {
        let ticket = super::random::<32>();
        let mut waiting = self.0.lock().unwrap_or_else(|e| e.into_inner());
        waiting.retain(|(_, _, at)| now.duration_since(*at) < TICKET_LIFE);
        if waiting.len() >= MAX_TICKETS {
            waiting.remove(0);
        }
        waiting.push((super::passkey::sha256(&ticket), session, now));
        data_encoding::BASE64URL_NOPAD.encode(&ticket)
    }

    /// Whether `ticket` is `session`'s and under 30 s old. It is spent either way.
    pub fn redeem(&self, ticket: &str, session: [u8; 32]) -> bool {
        self.redeem_at(ticket, session, Instant::now())
    }

    fn redeem_at(&self, ticket: &str, session: [u8; 32], now: Instant) -> bool {
        let Ok(ticket) = data_encoding::BASE64URL_NOPAD.decode(ticket.as_bytes()) else {
            return false;
        };
        let hash = super::passkey::sha256(&ticket);
        let mut waiting = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let Some(at) = (waiting.iter()).position(|(h, _, _)| bool::from(h.ct_eq(&hash))) else {
            return false;
        };
        let (_, owner, made) = waiting.remove(at);
        bool::from(owner.ct_eq(&session)) && now.duration_since(made) < TICKET_LIFE
    }
}

/// What the hub keeps of the takeover beside `desktop::Control`: the tickets, the one socket,
/// and what the watchdog counts from.
pub struct Vnc {
    pub tickets: Tickets,
    open: AtomicBool,
    seen: Mutex<Seen>,
    moved: tokio::sync::Notify,
}

/// When the owner last sent a key or the pointer, and when the socket closed (`None` while it
/// is open, and from a take until the first one opens).
#[derive(Clone, Copy)]
struct Seen {
    input: Instant,
    closed: Option<Instant>,
}

/// The one VNC socket, counted until dropped.
struct Open(Arc<super::Hub>);

impl Drop for Open {
    fn drop(&mut self) {
        let vnc = &self.0.vnc;
        vnc.note(|seen| seen.closed = Some(Instant::now()));
        vnc.open.store(false, Ordering::Release);
    }
}

impl Vnc {
    pub fn new() -> Vnc {
        let seen = Seen {
            input: Instant::now(),
            closed: None,
        };
        Vnc {
            tickets: Tickets::default(),
            open: AtomicBool::new(false),
            seen: Mutex::new(seen),
            moved: tokio::sync::Notify::new(),
        }
    }

    fn note(&self, f: impl FnOnce(&mut Seen)) {
        f(&mut self.seen.lock().unwrap_or_else(|e| e.into_inner()));
        self.moved.notify_one();
    }

    fn seen(&self) -> Seen {
        *self.seen.lock().unwrap_or_else(|e| e.into_inner())
    }
}

impl Default for Vnc {
    fn default() -> Self {
        Self::new()
    }
}

/// The owner (signed, `attach`) takes the desktop: the ticket that opens `/api/vnc` for this
/// session, or why not. A session that holds it already gets a fresh ticket, and the wait for
/// its closed socket starts over.
pub fn take(hub: &super::Hub, session: [u8; 32]) -> Result<String, &'static str> {
    hub.desktop.take(session, crate::now())?;
    hub.vnc.note(|seen| seen.closed = None);
    Ok(hub.vnc.tickets.issue(session))
}

/// Whether a client's frame is the owner acting: RFB's KeyEvent (4) or PointerEvent (5).
/// shortcut: reads only the first byte of a frame, as noVNC sends each message in a frame of its
/// own; a client that batches input behind a screen request would count as idle.
fn is_input(frame: &[u8]) -> bool {
    matches!(frame.first(), Some(4 | 5))
}

const IDLE: &str = "after 10 min without input";
const CAP: &str = "after 60 min";
const GONE: &str = "after the app closed";

/// When the desktop next goes back by itself, and why, for a takeover that began at `started`.
fn due(cfg: &Config, started: Instant, seen: Seen) -> (Instant, &'static str) {
    let mut first = (started + cfg.desktop_cap, CAP);
    let idle = seen.input.max(started) + cfg.desktop_idle;
    if idle < first.0 {
        first = (idle, IDLE);
    }
    if let Some(closed) = seen.closed {
        let gone = closed.max(started) + cfg.desktop_gone;
        if gone < first.0 {
            first = (gone, GONE);
        }
    }
    first
}

/// Gives the desktop back by itself (DESIGN §4.3.3): after 10 minutes with no input, 60 minutes
/// in all, or 60 s after the socket closed with no new take (a phone that switched apps), and
/// tells the owner. Without it a takeover nobody ends would stop the agent's browser for good.
/// Never returns.
pub(super) async fn watchdog(app: Arc<App>) {
    let hub = &app.hub;
    let mut changes = hub.desktop.changes();
    loop {
        let held = changes.borrow_and_update().clone();
        let Some(held) = held else {
            if changes.changed().await.is_err() {
                return std::future::pending().await;
            }
            continue;
        };
        let started = Instant::now();
        let why = loop {
            let (at, why) = due(&app.cfg(), started, hub.vnc.seen());
            if at <= Instant::now() {
                break Some(why);
            }
            tokio::select! {
                () = tokio::time::sleep_until(at.into()) => {}
                () = hub.vnc.moved.notified() => {}
                changed = changes.changed() => {
                    if changed.is_err() || changes.borrow().as_ref() != Some(&held) {
                        break None; // handed back
                    }
                }
            }
        };
        if let (Some(why), Some(_)) = (why, hub.desktop.give(crate::now())) {
            let text = format!("[zephyr] the desktop went back to the agent {why}");
            let told = crate::attach::AttachRequest::WebNotice(text);
            let _ = app.handles.router.send(told).await;
        }
    }
}

/// `GET /api/vnc`: the takeover's WebSocket, for the signed-in session that holds the desktop
/// (`admit` has checked its Origin). It offers `Sec-WebSocket-Protocol: zephyr-vnc, zt.<ticket>`
/// and is answered `zephyr-vnc`. One at a time, outside the chat sockets' count.
pub(super) fn ws(mut req: Request<Incoming>, app: &Arc<App>) -> Res {
    let Some(session) = app.session(req.headers()) else {
        return super::text(StatusCode::UNAUTHORIZED, "");
    };
    let Some(accept) = super::accept_key(req.headers()) else {
        return super::text(StatusCode::BAD_REQUEST, "");
    };
    let offered: Vec<String> = (req.headers().get_all(header::SEC_WEBSOCKET_PROTOCOL).iter())
        .filter_map(|v| v.to_str().ok())
        .flat_map(|v| v.split(','))
        .map(|p| p.trim().to_string())
        .collect();
    let ticket = offered.iter().find_map(|p| p.strip_prefix("zt."));
    let hub = &app.hub;
    let mine = |held: Option<crate::desktop::Held>| {
        held.is_some_and(|h| bool::from(h.session.ct_eq(&session.token_hash)))
    };
    if !offered.iter().any(|p| p == PROTOCOL)
        || !ticket.is_some_and(|t| hub.vnc.tickets.redeem(t, session.token_hash))
        || !mine(hub.desktop.held())
    {
        return super::text(StatusCode::UNAUTHORIZED, "");
    }
    if hub.vnc.open.swap(true, Ordering::AcqRel) {
        return super::text(StatusCode::SERVICE_UNAVAILABLE, "");
    }
    let open = Open(hub.clone());
    hub.vnc.note(|seen| seen.closed = None);
    let (upgrade, app) = (hyper::upgrade::on(&mut req), app.clone());
    tokio::spawn(async move {
        // Fails when the client went away before the 101 reached it.
        match upgrade.await {
            Ok(upgraded) => bridge(TokioIo::new(upgraded), session, &app, open).await,
            Err(_) => drop(open),
        }
    });
    let mut res = super::text(StatusCode::SWITCHING_PROTOCOLS, "");
    let h = res.headers_mut();
    h.insert(header::UPGRADE, HeaderValue::from_static("websocket"));
    h.insert(header::CONNECTION, HeaderValue::from_static("Upgrade"));
    h.insert(header::SEC_WEBSOCKET_ACCEPT, accept);
    h.insert(
        header::SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(PROTOCOL),
    );
    res
}

/// Starts x11vnc for one client, as the agent: it is an X client of the agent's own server, and
/// root speaking X to that would hand the agent a parser inside a root process. Its stdin and
/// stdout are one end of an unnamed socketpair; the other end is what comes back.
fn x11vnc(cfg: &Config) -> std::io::Result<(tokio::process::Child, tokio::net::UnixStream)> {
    let display = cfg.desktop.as_deref().unwrap_or_default();
    let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
    let mut cmd = std::process::Command::new(&cfg.x11vnc);
    cmd.args(X11VNC_ARGS)
        .args(["-display", display, "-auth"])
        .arg(cfg.home.join(crate::desktop::XAUTHORITY))
        .stdin(Stdio::from(std::os::fd::OwnedFd::from(theirs.try_clone()?)))
        .stdout(Stdio::from(std::os::fd::OwnedFd::from(theirs)))
        .stderr(Stdio::null());
    crate::agent::cmd(&mut cmd);
    let mut cmd = tokio::process::Command::from(cmd);
    let child = cmd.kill_on_drop(true).spawn()?;
    ours.set_nonblocking(true)?;
    Ok((child, tokio::net::UnixStream::from_std(ours)?))
}

/// Joins the WebSocket's binary frames to x11vnc, with no cap on how many (a pointer drag sends
/// hundreds). Closed by a text frame, a frame over 1 MiB, x11vnc ending, the session ending,
/// and the hand back; x11vnc is killed with it.
async fn bridge<S>(io: S, session: WebSession, app: &App, open: Open)
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let limits = WebSocketConfig {
        max_message_size: Some(MAX_FRAME),
        max_frame_size: Some(MAX_FRAME),
        ..Default::default()
    };
    let ws = WebSocketStream::from_raw_socket(io, Role::Server, Some(limits)).await;
    let (mut sink, mut frames) = ws.split();
    let hub = &app.hub;
    let started = x11vnc(&app.cfg());
    if let Err(e) = &started {
        eprintln!("zephyr: starting x11vnc for the takeover: {e}");
    }
    if let Ok((child, sock)) = started {
        let (mut from_vnc, mut to_vnc) = sock.into_split();
        let mut held = hub.desktop.changes();
        let mut revoked = hub.revocations();
        let mut checks = tokio::time::interval(super::SESSION_CHECK);
        let mut buf = vec![0u8; 64 * 1024];
        let mine = |held: &Option<crate::desktop::Held>| {
            (held.as_ref()).is_some_and(|h| h.session == session.token_hash)
        };
        loop {
            // Looked at before each wait: a hand back while this socket was being set up came
            // before `held` was listening for it.
            if !mine(&held.borrow_and_update()) {
                break;
            }
            tokio::select! {
                frame = frames.next() => match frame {
                    Some(Ok(Message::Binary(bytes))) => {
                        if is_input(&bytes) {
                            hub.vnc.note(|seen| seen.input = Instant::now());
                        }
                        let sent = tokio::time::timeout(WRITE_WAIT, to_vnc.write_all(&bytes));
                        if !matches!(sent.await, Ok(Ok(()))) {
                            break;
                        }
                    }
                    // tungstenite answers a ping itself.
                    Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                    _ => break, // text, a close, an oversized frame, the end
                },
                read = from_vnc.read(&mut buf) => match read {
                    Ok(n) if n > 0 => {
                        let out = Message::Binary(buf[..n].to_vec());
                        let sent = tokio::time::timeout(WRITE_WAIT, sink.send(out));
                        if !matches!(sent.await, Ok(Ok(()))) {
                            break;
                        }
                    }
                    _ => break, // x11vnc ended
                },
                changed = held.changed() => {
                    if changed.is_err() {
                        break;
                    }
                }
                _ = checks.tick() => {
                    if app.live(&session.token_hash).is_none() {
                        break;
                    }
                }
                changed = revoked.changed() => {
                    if changed.is_err() || app.live(&session.token_hash).is_none() {
                        break;
                    }
                }
            }
        }
        drop(child); // killed
    }
    // Before the close reaches the client: its next socket finds the place free.
    drop(open);
    let _ = tokio::time::timeout(WRITE_WAIT, sink.close()).await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_is_its_sessions_for_30_s_and_only_its_hash_is_kept() {
        let tickets = Tickets::default();
        let now = Instant::now();
        let (me, other) = ([1; 32], [2; 32]);
        let ticket = tickets.issue_at(me, now);
        let raw = data_encoding::BASE64URL_NOPAD
            .decode(ticket.as_bytes())
            .unwrap();
        assert_eq!(raw.len(), 32);
        {
            let kept = tickets.0.lock().unwrap();
            assert_eq!(kept.len(), 1);
            assert_eq!(kept[0].0, crate::web::passkey::sha256(&raw));
            assert_ne!(&kept[0].0[..], &raw[..], "the ticket itself is kept");
        }
        assert!(tickets.redeem_at(&ticket, me, now + Duration::from_secs(29)));
        assert!(!tickets.redeem_at(&ticket, me, now), "used");
        // Another session's is refused, and spent.
        let ticket = tickets.issue_at(me, now);
        assert!(!tickets.redeem_at(&ticket, other, now));
        assert!(!tickets.redeem_at(&ticket, me, now));
        // 31 s old.
        let ticket = tickets.issue_at(me, now);
        assert!(!tickets.redeem_at(&ticket, me, now + Duration::from_secs(31)));
        for junk in ["", "zt.", "not base64!", &"A".repeat(43)] {
            assert!(!tickets.redeem_at(junk, me, now), "{junk}");
        }
        // A flood of asking keeps 16, the newest.
        let all: Vec<String> = (0..20).map(|_| tickets.issue_at(me, now)).collect();
        assert_eq!(tickets.0.lock().unwrap().len(), MAX_TICKETS);
        assert!(!tickets.redeem_at(&all[0], me, now));
        assert!(tickets.redeem_at(&all[19], me, now));
    }

    #[test]
    fn x11vnc_never_gets_a_flag_that_listens() {
        assert_eq!(X11VNC_ARGS[0], "-inetd");
        for flag in X11VNC_ARGS {
            for listens in [
                "listen", "rfbport", "autoport", "unixsock", "connect", "http",
            ] {
                assert!(!flag.contains(listens), "{flag}");
            }
        }
        // By full path, never one the agent left on its PATH.
        let cfg: Config = toml::from_str("rpi = '/r'\nhome = '/h'\n").unwrap();
        assert_eq!(cfg.x11vnc, std::path::Path::new("/usr/bin/x11vnc"));
    }

    #[test]
    fn the_desktop_goes_back_after_no_input_the_cap_or_a_closed_socket() {
        let cfg: Config = toml::from_str("rpi = '/r'\nhome = '/h'\n").unwrap();
        let (min, started) = (Duration::from_secs(60), Instant::now());
        let seen = |input: u32, closed: Option<u32>| Seen {
            input: started + min * input,
            closed: closed.map(|c| started + min * c),
        };
        assert_eq!(
            due(&cfg, started, seen(0, None)),
            (started + min * 10, IDLE)
        );
        // Input moves it; an input from before this takeover does not shorten it.
        assert_eq!(
            due(&cfg, started, seen(7, None)),
            (started + min * 17, IDLE)
        );
        let before = Seen {
            input: started - min * 5,
            closed: Some(started - min * 5),
        };
        assert_eq!(
            due(
                &cfg,
                started,
                Seen {
                    closed: None,
                    ..before
                }
            ),
            (started + min * 10, IDLE)
        );
        assert_eq!(due(&cfg, started, before), (started + min, GONE));
        assert_eq!(
            due(&cfg, started, seen(3, Some(3))),
            (started + min * 4, GONE)
        );
        // 60 minutes in all, whatever the owner does.
        assert_eq!(
            due(&cfg, started, seen(55, None)),
            (started + min * 60, CAP)
        );
        assert_eq!(
            due(&cfg, started, seen(59, Some(59))),
            (started + min * 60, CAP)
        );
    }

    #[test]
    fn only_keys_and_the_pointer_count_as_input() {
        assert!(is_input(&[4, 1, 0, 0, 0, 0, 0, 97]) && is_input(&[5, 0, 0, 1, 0, 1]));
        // A screen request, the handshake's version and the client's init are not.
        assert!(!is_input(&[3, 1, 0, 0, 0, 0, 4, 0, 3, 0]));
        assert!(!is_input(b"RFB 003.008\n") && !is_input(&[1]) && !is_input(&[]));
    }
}
