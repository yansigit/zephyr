//! Who may use the web app (DESIGN §4.3.3): enrolling a passkey with a one-time code from a
//! strong channel, signing in with it, the session cookie, the limits on both ceremonies, and
//! the owner's `/web` commands. A code, a cookie's token and a challenge never reach a log.

use super::{error, json, passkey, App, Res, Window, COOKIE};
use crate::attach::AttachRequest;
use crate::config::Config;
use crate::store::{Store, MAX_PASSKEYS};
use data_encoding::{BASE32_NOPAD, BASE64URL_NOPAD as B64, HEXLOWER};
use hyper::header::{self, HeaderMap, HeaderValue};
use hyper::StatusCode;
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// How long an enrolment code works.
const CODE_LIFE: i64 = 10 * 60;
/// Failed ceremonies within `FAILURE_SPAN` that close enrolment and sign-in.
const MAX_FAILURES: usize = 5;
const FAILURE_SPAN: Duration = Duration::from_secs(10 * 60);
/// Before sign-in there is no session to own a challenge.
const NOBODY: [u8; 32] = [0; 32];
const USAGE: &str = "[zephyr] usage: /web | /web enrol [NAME] | /web rm NAME | /web signout";
const WRONG_CODE: &str = "that code is wrong, used or expired";
const NOT_VERIFIED: &str = "that did not verify; start again";

/// A ceremony between its start and its finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ceremony {
    /// Started with the enrolment code that has this hash.
    Enrol {
        code_sha256: [u8; 32],
    },
    Login,
}

/// The limits on the two ceremonies. Global and in memory: every request comes from 127.0.0.1.
pub(super) struct Limits {
    requests: Window,
    failures: VecDeque<Instant>,
    /// While sign-in is closed: until when, and that as a unix time for the owner.
    closed: Option<(Instant, i64)>,
}

impl Limits {
    pub(super) fn new() -> Self {
        Self {
            requests: Window::new(20, Duration::from_secs(60)),
            failures: VecDeque::new(),
            closed: None,
        }
    }

    /// When sign-in opens again, while it is closed.
    fn closed(&mut self, now: Instant) -> Option<i64> {
        self.closed = self.closed.filter(|(until, _)| now < *until);
        self.closed.map(|(_, at)| at)
    }

    /// Counts a failed ceremony. The fifth within ten minutes closes sign-in for `lockout`;
    /// only that call answers, with the time it opens again.
    fn failed(&mut self, now: Instant, lockout: Duration) -> Option<i64> {
        self.failures.push_back(now);
        while (self.failures.front()).is_some_and(|t| now.duration_since(*t) > FAILURE_SPAN) {
            self.failures.pop_front();
        }
        if self.failures.len() < MAX_FAILURES {
            return None;
        }
        self.failures.clear();
        let at = crate::now() + lockout.as_secs() as i64;
        self.closed = Some((now + lockout, at));
        Some(at)
    }
}

pub(crate) fn random<const N: usize>() -> [u8; N] {
    use ring::rand::SecureRandom;
    let mut out = [0; N];
    ring::rand::SystemRandom::new()
        .fill(&mut out)
        .expect("the system's random source");
    out
}

/// The `Set-Cookie` value for a session's token. Only the token's SHA-256 is stored.
pub fn cookie(token: &[u8; 32]) -> String {
    let token = B64.encode(token);
    format!("{COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=2592000")
}

/// A passkey's name: 1 to 32 of `[A-Za-z0-9_-]`.
fn named(name: &str) -> bool {
    (1..=32).contains(&name.len())
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// A code as typed: uppercase, without its hyphen or any space.
fn typed(code: &str) -> String {
    let kept = code.chars().filter(|c| *c != '-' && !c.is_whitespace());
    kept.flat_map(char::to_uppercase).collect()
}

/// The enrolment code that still works: `meta`'s row as stored, the code's hash and the name
/// it was made for.
fn pending_code(store: &Store, now: i64) -> Option<(String, [u8; 32], Option<String>)> {
    let raw = store.meta("web_enrol").ok()??;
    let v: Value = serde_json::from_str(&raw).ok()?;
    let hash = HEXLOWER.decode(v["sha256"].as_str()?.as_bytes()).ok()?;
    let live = v["expires"].as_i64()? > now;
    let name = v["name"].as_str().map(str::to_string);
    live.then_some((raw, hash.try_into().ok()?, name))
}

/// `secs` as the owner reads an age: `40s`, `5m`, `2h`, `3d`.
fn span(secs: i64) -> String {
    match secs.max(0) {
        s if s < 60 => format!("{s}s"),
        s if s < 3600 => format!("{}m", s / 60),
        s if s < 86_400 => format!("{}h", s / 3600),
        s => format!("{}d", s / 86_400),
    }
}

fn count(n: usize, one: &str) -> String {
    format!("{n} {one}{}", if n == 1 { "" } else { "s" })
}

/// The owner's `/web` commands (DESIGN §4.3.3), on `channel`: the reply, and whether sessions
/// were ended, so the router has open sockets check theirs. `None` when `text` is not one.
pub fn command(cfg: &Config, store: &Store, text: &str, channel: &str) -> Option<(String, bool)> {
    let (cmd, arg) = text.split_once(' ').unwrap_or((text, ""));
    if cmd != "/web" {
        return None;
    }
    let Some(web) = &cfg.web else {
        let off = "[zephyr] the web app is not set up; zephyr web setup on the host";
        return Some((off.into(), false));
    };
    let failed =
        |e: rusqlite::Error| format!("[zephyr] reading the web app's passkeys failed: {e}");
    let words: Vec<&str> = arg.split_whitespace().collect();
    let done = match words[..] {
        [] => listing(web, store).map(|line| (line, false)),
        ["enrol"] => enrol(cfg, web, store, None, channel).map(|line| (line, false)),
        ["enrol", name] if named(name) => {
            enrol(cfg, web, store, Some(name), channel).map(|line| (line, false))
        }
        ["rm", name] => store.remove_passkey(name).map(|removed| match removed {
            Some(sessions) => {
                let line = format!(
                    "[zephyr] removed passkey {name} and its {sessions} sessions and pushes"
                );
                (line, true)
            }
            None => (
                format!("[zephyr] no passkey {name}; /web lists them"),
                false,
            ),
        }),
        ["signout"] => {
            (store.end_web_sessions()).map(|n| (format!("[zephyr] ended {n} web sessions"), true))
        }
        _ => Ok((USAGE.into(), false)),
    };
    Some(done.unwrap_or_else(|e| (failed(e), false)))
}

fn listing(web: &crate::config::Web, store: &Store) -> rusqlite::Result<String> {
    let now = crate::now();
    let keys: Vec<String> = (store.passkeys()?.iter())
        .map(|p| match p.last_used_at {
            Some(at) => format!("{} (used {} ago)", p.name, span(now - at)),
            None => p.name.clone(),
        })
        .collect();
    let keys = match keys.is_empty() {
        true => "none".to_string(),
        false => keys.join(", "),
    };
    Ok(format!(
        "[zephyr] web app https://{} · passkeys: {keys} · {} · push on {}",
        web.host,
        count(store.web_sessions()?.len(), "session"),
        count(store.pushes()?.len(), "device"),
    ))
}

/// `/web enrol [NAME]`: a new one-time code, replacing any before it. Only from a strong
/// channel that is not itself the passkey: a new passkey is as strong as what vouched for it.
fn enrol(
    cfg: &Config,
    web: &crate::config::Web,
    store: &Store,
    name: Option<&str>,
    channel: &str,
) -> rusqlite::Result<String> {
    let strong: Vec<&str> = (cfg.settings.strong_channels.iter())
        .map(String::as_str)
        .filter(|c| *c != "passkey")
        .collect();
    if !strong.contains(&channel) {
        return Ok(format!("[zephyr] /web enrol needs {}", strong.join(" or ")));
    }
    let keys = store.passkeys()?;
    if keys.len() >= MAX_PASSKEYS {
        return Ok(format!(
            "[zephyr] {MAX_PASSKEYS} passkeys is the most; /web rm NAME first"
        ));
    }
    if let Some(taken) = name.filter(|n| keys.iter().any(|k| k.name == *n)) {
        return Ok(format!(
            "[zephyr] a passkey named {taken} exists; /web rm {taken} first"
        ));
    }
    // 6 random bytes: 48 bits, 10 characters a phone can type.
    let code = BASE32_NOPAD.encode(&random::<6>());
    let row = json!({
        "sha256": HEXLOWER.encode(&passkey::sha256(code.as_bytes())),
        "name": name,
        "expires": crate::now() + CODE_LIFE,
    });
    store.set_meta("web_enrol", Some(&row.to_string()))?;
    Ok(format!(
        "[zephyr] open https://{}/#enrol={}-{} on the phone within 10 min, or type the code there; it works once",
        web.host,
        &code[..5],
        &code[5..]
    ))
}

/// `zephyr.web_devices`: the passkeys, sessions and push subscriptions, by name and time. No
/// credential id, key, cookie hash or endpoint.
pub fn devices(cfg: &Config, this: &[u8; 32]) -> Value {
    let read = || -> rusqlite::Result<Value> {
        let store = Store::open(&cfg.db_path())?;
        let keys = store.passkeys()?;
        let name = |id: i64| keys.iter().find(|k| k.id == id).map(|k| k.name.clone());
        let sessions: Vec<Value> = (store.web_sessions()?.iter())
            .map(|s| {
                json!({"passkey": name(s.passkey), "created_at": s.created_at,
                    "seen_at": s.seen_at, "this": s.token_hash == *this})
            })
            .collect();
        let push: Vec<Value> = (store.pushes()?.iter())
            .map(|p| json!({"passkey": name(p.passkey), "created_at": p.created_at}))
            .collect();
        let passkeys: Vec<Value> = (keys.iter())
            .map(|k| json!({"name": k.name, "created_at": k.created_at, "last_used_at": k.last_used_at}))
            .collect();
        Ok(json!({"passkeys": passkeys, "sessions": sessions, "push": push}))
    };
    read().unwrap_or_else(|e| {
        eprintln!("zephyr: reading the web app's devices failed: {e}");
        json!({"passkeys": [], "sessions": [], "push": []})
    })
}

impl App {
    fn store(&self) -> Option<Store> {
        (Store::open(&self.cfg().db_path()))
            .inspect_err(|e| eprintln!("zephyr: the web app could not open state.db: {e}"))
            .ok()
    }

    fn clock(&self, at: i64) -> String {
        let tz = crate::scheduler::zone(&self.cfg()).unwrap_or(jiff::tz::TimeZone::UTC);
        crate::settings::clock(at, &tz)
    }

    /// Before a ceremony: the 429 to answer while sign-in is closed (not counted) or past 20
    /// requests a minute.
    pub(super) fn closed(&self, counted: bool) -> Option<Res> {
        let mut limits = self.hub.limits.lock().unwrap_or_else(|e| e.into_inner());
        let now = Instant::now();
        if let Some(at) = limits.closed(now) {
            let why = format!("sign-in is closed until {}", self.clock(at));
            return Some(error(StatusCode::TOO_MANY_REQUESTS, &why));
        }
        // A sign-in merely started guesses nothing, so it is not an attempt: counting it let
        // anything at the port use up the owner's minute. `login_start` bounds those itself.
        let over = counted && !limits.requests.hit(now);
        over.then(|| {
            error(
                StatusCode::TOO_MANY_REQUESTS,
                "too many attempts; wait a minute",
            )
        })
    }

    /// A failed ceremony: counted, and the owner told once when it closes sign-in. `reason` is
    /// one of `passkey`'s fixed refusals, for the log; `said` is what the page shows.
    async fn refuse(&self, reason: &'static str, said: &'static str) -> Res {
        eprintln!("zephyr: a web app sign-in was refused: {reason}");
        let lockout = self.cfg().web_lockout;
        let closed = {
            let mut limits = self.hub.limits.lock().unwrap_or_else(|e| e.into_inner());
            limits.failed(Instant::now(), lockout)
        };
        if let Some(at) = closed {
            let until = self.clock(at);
            self.notice(format!(
                "[zephyr] 5 failed passkey attempts on the web app; sign-in is closed until {until}"
            ))
            .await;
        }
        error(StatusCode::FORBIDDEN, said)
    }

    /// A notice to every attached client and the owner last answered.
    async fn notice(&self, text: String) {
        let _ = self
            .handles
            .router
            .send(AttachRequest::WebNotice(text))
            .await;
    }

    /// Starts a session for a passkey: 200 `{"name"}` with its cookie.
    fn signed_in(&self, store: &Store, id: i64, name: &str) -> Res {
        let token: [u8; 32] = random();
        let hash = passkey::sha256(&token);
        let cookie = (store.add_web_session(&hash, id, crate::now()).ok())
            .and_then(|()| HeaderValue::from_str(&cookie(&token)).ok());
        let Some(cookie) = cookie else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        };
        let mut res = json(StatusCode::OK, json!({ "name": name }));
        res.headers_mut().insert(header::SET_COOKIE, cookie);
        res
    }

    /// `POST /api/enrol/start {code}`: the options for `navigator.credentials.create`.
    pub(super) async fn enrol_start(&self, body: &Value) -> Res {
        let Some(store) = self.store() else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        };
        let code = passkey::sha256(typed(body["code"].as_str().unwrap_or_default()).as_bytes());
        let pending = pending_code(&store, crate::now());
        let Some((_, hash, _)) = pending.filter(|(_, hash, _)| bool::from(hash.ct_eq(&code)))
        else {
            return self.refuse("wrong enrolment code", WRONG_CODE).await;
        };
        // The install's own handle: every passkey is the one owner's.
        let user = match store.meta("web_user") {
            Ok(Some(user)) => HEXLOWER.decode(user.as_bytes()).unwrap_or_default(),
            _ => {
                let user = random::<16>();
                let _ = store.set_meta("web_user", Some(&HEXLOWER.encode(&user)));
                user.to_vec()
            }
        };
        let exclude: Vec<Value> = (store.passkeys().unwrap_or_default().iter())
            .map(|k| json!({"type": "public-key", "id": B64.encode(&k.cred_id)}))
            .collect();
        let challenge: [u8; 32] = random();
        let ceremony = Ceremony::Enrol { code_sha256: hash };
        let id = self.hub.ceremonies.issue(NOBODY, ceremony, challenge);
        let host = &self.web.host;
        json(
            StatusCode::OK,
            json!({"challengeId": id, "publicKey": {
                "rp": {"id": host, "name": "zephyr"},
                "user": {"id": B64.encode(&user), "name": "zephyr owner", "displayName": "zephyr owner"},
                "challenge": B64.encode(&challenge),
                "pubKeyCredParams": [{"type": "public-key", "alg": -7}],
                "authenticatorSelection": {"residentKey": "required", "userVerification": "required"},
                "attestation": "none",
                "timeout": 120_000,
                "excludeCredentials": exclude,
            }}),
        )
    }

    /// `POST /api/enrol/finish`: checks the registration, uses the code up, stores the passkey
    /// and signs the browser in.
    pub(super) async fn enrol_finish(&self, body: &Value) -> Res {
        let Some(store) = self.store() else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        };
        let id = body["challengeId"].as_str().unwrap_or_default();
        let Some((Ceremony::Enrol { code_sha256 }, challenge)) =
            self.hub.ceremonies.take(id, NOBODY)
        else {
            return self.refuse("stale challenge", NOT_VERIFIED).await;
        };
        // The code its ceremony started with must still be the one that works.
        let pending = pending_code(&store, crate::now());
        let Some((row, _, code_name)) =
            pending.filter(|(_, hash, _)| bool::from(hash.ct_eq(&code_sha256)))
        else {
            return self.refuse("wrong enrolment code", WRONG_CODE).await;
        };
        let checked = passkey::created(body).and_then(|c| {
            Ok((
                passkey::check_created(&self.web.host, &challenge, &c)?,
                c.id,
            ))
        });
        let (point, cred_id) = match checked {
            Ok(checked) => checked,
            Err(reason) => return self.refuse(reason, NOT_VERIFIED).await,
        };
        let keys = store.passkeys().unwrap_or_default();
        if keys.len() >= MAX_PASSKEYS {
            let why = format!("{MAX_PASSKEYS} passkeys is the most; /web rm NAME first");
            return error(StatusCode::CONFLICT, &why);
        }
        let free = || {
            (1..)
                .map(|n| format!("passkey-{n}"))
                .find(|name| keys.iter().all(|k| k.name != *name))
        };
        let asked = body["name"]
            .as_str()
            .filter(|n| named(n))
            .map(str::to_string);
        let Some(name) = code_name.or(asked).or_else(free) else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        };
        // One code, one passkey: of two ceremonies started with it, only one takes it.
        if !store.take_meta("web_enrol", &row).unwrap_or(false) {
            return self.refuse("wrong enrolment code", WRONG_CODE).await;
        }
        let Ok(id) = store.add_passkey(&name, &cred_id, &point) else {
            return error(
                StatusCode::CONFLICT,
                "that passkey or its name is already enrolled",
            );
        };
        let res = self.signed_in(&store, id, &name);
        // How the owner finds out if someone else used the code.
        self.notice(format!(
            "[zephyr] passkey {name} was added to the web app; /web rm {name} removes it"
        ))
        .await;
        res
    }

    /// `POST /api/login/start {}`: the options for `navigator.credentials.get`. No credential
    /// is named: the passkey is discoverable.
    pub(super) fn login_start(&self) -> Res {
        let challenge: [u8; 32] = random();
        // Anyone at the port may start a sign-in. When 16 wait, a new one is turned away
        // rather than push out one in flight, which may be the owner's.
        let issued = (self.hub.ceremonies).try_issue(NOBODY, Ceremony::Login, challenge);
        let Some(id) = issued else {
            let why = "too many sign-ins are waiting; try again in two minutes";
            return error(StatusCode::TOO_MANY_REQUESTS, why);
        };
        json(
            StatusCode::OK,
            json!({"challengeId": id, "publicKey": {"challenge": B64.encode(&challenge),
                "rpId": self.web.host, "timeout": 120_000, "userVerification": "required"}}),
        )
    }

    /// `POST /api/login/finish`: checks the assertion under the stored key and signs in.
    pub(super) async fn login_finish(&self, body: &Value) -> Res {
        let Some(store) = self.store() else {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        };
        let id = body["challengeId"].as_str().unwrap_or_default();
        let Some((Ceremony::Login, challenge)) = self.hub.ceremonies.take(id, NOBODY) else {
            return self.refuse("stale challenge", NOT_VERIFIED).await;
        };
        let verified = passkey::assertion(body).and_then(|a| {
            let key = (store.passkey_by_cred(&a.id).ok().flatten()).ok_or("unknown credential")?;
            let count = passkey::verify(
                &self.web.host,
                &challenge,
                &key.public_key,
                key.sign_count,
                &a,
            )?;
            Ok((key, count))
        });
        let (key, count) = match verified {
            Ok(verified) => verified,
            Err(reason) => return self.refuse(reason, NOT_VERIFIED).await,
        };
        if let Err(e) = store.passkey_used(key.id, count, crate::now()) {
            eprintln!("zephyr: marking a passkey used failed: {e}");
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        }
        self.signed_in(&store, key.id, &key.name)
    }

    /// `POST /api/logout`: ends this session, closes its socket and clears the cookie.
    pub(super) fn logout(&self, h: &HeaderMap) -> Res {
        let Some(session) = self.session(h) else {
            return error(StatusCode::UNAUTHORIZED, "sign in first");
        };
        let ended = self.store().map(|s| s.end_web_session(&session.token_hash));
        if !matches!(ended, Some(Ok(()))) {
            return error(StatusCode::INTERNAL_SERVER_ERROR, "try again");
        }
        self.hub.revoke();
        let mut res = super::text(StatusCode::NO_CONTENT, "");
        let clear = "__Host-zephyr=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0";
        (res.headers_mut()).insert(header::SET_COOKIE, HeaderValue::from_static(clear));
        res
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn five_failures_in_ten_minutes_close_sign_in_for_the_lockout() {
        let (start, lockout) = (Instant::now(), Duration::from_secs(900));
        let at = |secs| start + Duration::from_secs(secs);
        let mut l = Limits::new();
        assert_eq!(l.closed(start), None);
        for n in 0..4 {
            assert_eq!(l.failed(at(n), lockout), None);
        }
        // The first four are over ten minutes old by now: this is the first of a new five.
        assert_eq!(l.failed(at(700), lockout), None);
        for n in 1..4 {
            assert_eq!(l.failed(at(700 + n), lockout), None);
        }
        let until = l.failed(at(704), lockout).expect("the fifth closes it");
        assert!((until - crate::now() - 900).abs() <= 1);
        assert_eq!(l.closed(at(705)), Some(until));
        assert_eq!(l.closed(at(704 + 899)), Some(until));
        assert_eq!(l.closed(at(704 + 900)), None, "open again");
        assert_eq!(l.failed(at(1700), lockout), None, "and counting from none");
    }

    #[test]
    fn a_typed_code_a_name_and_a_cookie_are_read_as_written() {
        assert_eq!(typed(" abcde - fghjk\n"), "ABCDEFGHJK");
        assert_eq!(typed("ABCDE-FGHJK"), "ABCDEFGHJK");
        for ok in ["phone", "my_iPad-2", "a", &"x".repeat(32)] {
            assert!(named(ok), "{ok}");
        }
        for bad in ["", "two words", "a/b", "é", &"x".repeat(33)] {
            assert!(!named(bad), "{bad}");
        }
        assert_eq!(
            cookie(&[0; 32]),
            "__Host-zephyr=AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=2592000"
        );
        assert_eq!(
            [span(40), span(300), span(7200), span(3 * 86_400)],
            ["40s", "5m", "2h", "3d"]
        );
        assert_eq!(
            [count(1, "session"), count(0, "device")],
            ["1 session", "0 devices"]
        );
    }
}
