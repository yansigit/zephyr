//! Telnyx, the one telephony provider built (DESIGN §4.26): Call Control over the daemon's own
//! HTTPS client, and the check that a media connection or a webhook is Telnyx's.
//!
//! Written against Telnyx's published documentation, read on 2026-11-11:
//!
//! - Dial: `POST /v2/calls`, <https://developers.telnyx.com/api-reference/call-commands/dial>.
//!   Required `connection_id`, `to`, `from`; `timeout_secs` (5 to 600, default 30);
//!   `time_limit_secs` (30 to 14400); `stream_url`; `stream_track` (`inbound_track` is the
//!   other party only); `stream_bidirectional_mode: "rtp"` with `stream_bidirectional_codec:
//!   "PCMU"` to send audio back; `webhook_url`. The answer is `data.call_control_id`.
//!   `record` is never sent, so nothing is recorded.
//! - Hang up: `POST /v2/calls/{call_control_id}/actions/hangup`.
//! - DTMF: `POST /v2/calls/{call_control_id}/actions/send_dtmf` with `digits`,
//!   <https://developers.telnyx.com/api-reference/call-commands/send-dtmf>.
//! - Media streaming: <https://developers.telnyx.com/docs/voice/programmable-voice/media-streaming>.
//!   Telnyx opens a WebSocket to `stream_url` and sends JSON text frames: `connected`, `start`,
//!   `media` (`media.payload`: base64 of the RTP payload without headers, so G.711 μ-law at
//!   8 kHz), `stop`, `mark`, `dtmf`, `error`. Sent back: `{"event":"media","media":{"payload"}}`
//!   (chunks of 20 ms to 30 s), `{"event":"mark","mark":{"name"}}` (echoed once the audio
//!   before it has played) and `{"event":"clear"}` (stops playback, empties the queue).
//! - The signature, same page: every media connection carries `telnyx-signature-ed25519`
//!   (base64) and `telnyx-timestamp` (Unix seconds); the signed string is
//!   `{telnyx-timestamp}|{stream_url}`, checked with the account's ed25519 public key (the
//!   Mission Control Portal, API keys), "the same key pair Telnyx uses for webhook
//!   signatures", where the signed string is `{timestamp}|{body}`.
//!
//! - Telnyx's own speech (`calls.speech = "provider"`), read on 2026-11-11:
//!   - Speak: `POST /v2/calls/{call_control_id}/actions/speak`,
//!     <https://developers.telnyx.com/api-reference/call-commands/speak-text>: `payload` (text,
//!     at most 3000 characters), `voice` (`AWS.Polly.<VoiceId>`, `-Neural` for a neural
//!     voice), `language` (`en-US`, `fr-CA`, `ko-KR` among them; ignored for a Polly voice,
//!     which has its own). Consecutive speak commands are queued, not interrupted; the
//!     webhooks are `call.speak.started` and `call.speak.ended`.
//!   - Transcription: `POST /v2/calls/{call_control_id}/actions/transcription_start`,
//!     <https://developers.telnyx.com/api-reference/call-commands/transcription-start>:
//!     `transcription_engine` (`Telnyx` here: its config takes `language` `en`, `fr`, `ko`
//!     and sends final results only), `transcription_tracks` (`inbound`). Results come as
//!     `call.transcription` webhooks; transcription stops at the hang-up.
//!
//!   The text goes through the daemon both ways, exactly as with whisper and Piper: only who
//!   turns sound into text and back changes. Telnyx then hears the call's audio.
//!
//! `ASSUMPTION(live)` marks what the documentation leaves open and a first real call must
//! confirm.

use crate::config::Config;
use crate::vault::Service;
use base64::Engine;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Duration;

const B64: base64::engine::GeneralPurpose = base64::engine::general_purpose::STANDARD;

/// Telnyx's own API.
pub const API: &str = "https://api.telnyx.com/v2";
/// The vault's `[[service]]` this reads.
pub const SERVICE: &str = "calls";
/// How long the other side's phone rings before Telnyx gives up (its default).
pub const RING_SECS: u64 = 30;
/// How far a signature's timestamp may be from now.
/// ASSUMPTION(live): Telnyx's docs name no window; five minutes is what its webhook guide and
/// SDKs use.
const TOLERANCE: i64 = 300;
/// How long a signature is remembered, so it is taken once: past this its timestamp is stale.
const REMEMBER: i64 = 2 * TOLERANCE;
const HTTP_WAIT: Duration = Duration::from_secs(15);

/// What a provider does for a call. Telnyx is the one built; another is one more `impl`.
pub trait Telephony {
    /// Places the call; the provider connects its media to `stream_url` and posts the call's
    /// events to `events_url`. Returns the provider's id for the call.
    fn dial(&self, d: &Dial) -> impl std::future::Future<Output = Result<String, String>> + Send;
    fn hangup(&self, call: &str) -> impl std::future::Future<Output = Result<(), String>> + Send;
    /// Speaks `text` on the call in `language` (`en`, `fr`, `ko`), after what was asked before.
    fn speak(
        &self,
        call: &str,
        text: &str,
        language: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
    /// Starts transcribing what the other party says, in `language`.
    fn transcribe(
        &self,
        call: &str,
        language: &str,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
    /// Presses one key (`0-9`, `*`, `#`) in a phone menu.
    fn dtmf(
        &self,
        call: &str,
        digit: char,
    ) -> impl std::future::Future<Output = Result<(), String>> + Send;
    /// Whether `signature` over `timestamp` and `signed` is the provider's, fresh, and not seen
    /// before.
    fn verify(&self, timestamp: &str, signature: &str, signed: &[u8], now: i64) -> bool;
}

/// One call to place.
pub struct Dial {
    /// `+14165550123`.
    pub to: String,
    /// `wss://host/calls/<token>`: exactly what Telnyx signs. `None` when Telnyx does the
    /// speech itself: then no audio is streamed to the daemon at all.
    pub stream_url: Option<String>,
    /// `https://host/calls/events`.
    pub events_url: String,
    /// The provider ends the call itself after this long: a backstop behind the daemon's cap.
    pub limit_secs: u64,
}

/// The daemon's Telnyx client. It holds no key: the vault is read for each request, so a key
/// the owner changed or removed applies at once.
#[derive(Clone)]
pub struct Telnyx {
    vault: PathBuf,
    /// The API's base address, `Config::calls_api`: Telnyx's own but in tests.
    api: String,
    http: reqwest::Client,
    /// The account's public key as the vault had it when the file was last changed: a request
    /// on the public listener is checked without reading the vault again.
    public_key: std::sync::Arc<Mutex<Option<(std::time::SystemTime, String)>>>,
}

/// The voice and the language tag Telnyx speaks a call's language with: Amazon Polly's neural
/// voices, Canadian for French.
/// ASSUMPTION(live): these voice ids are offered on the owner's Telnyx account.
fn voice(language: &str) -> (&'static str, &'static str) {
    match language {
        "fr" => ("AWS.Polly.Gabrielle-Neural", "fr-CA"),
        "ko" => ("AWS.Polly.Seoyeon-Neural", "ko-KR"),
        _ => ("AWS.Polly.Joanna-Neural", "en-US"),
    }
}

/// Why a call cannot be placed at all.
pub const NOT_SET_UP: &str = "not run: the call provider is not set up (zephyr vault add \
                              service calls on the host)";

impl Telnyx {
    /// The client. Whether the vault has a usable `calls` service is found at each request.
    pub fn new(cfg: &Config) -> Telnyx {
        let http = reqwest::Client::builder().timeout(HTTP_WAIT).build();
        Telnyx {
            vault: crate::vault::path(cfg),
            api: cfg.calls_api.clone(),
            // The builder fails only when the TLS backend cannot start, as `Client::new` would.
            http: http.unwrap_or_default(),
            public_key: Default::default(),
        }
    }

    /// The client, once the vault has a usable `calls` service.
    pub fn load(cfg: &Config) -> Result<Telnyx, String> {
        let telnyx = Telnyx::new(cfg);
        telnyx.service().map(|_| telnyx)
    }

    /// The calls service, checked as the owner's save was (`Service::usable`), with its key
    /// wiped: the ids a request needs, never the secret.
    fn service(&self) -> Result<Service, String> {
        let mut s = self.read()?;
        crate::vault::wipe_str(&mut s.key);
        s.key.clear();
        Ok(s)
    }

    /// The calls service as the vault has it now, key and all; [`Key`] and [`Telnyx::service`]
    /// take it apart at once.
    fn read(&self) -> Result<Service, String> {
        let vault = crate::vault::Vault::load(&self.vault).map_err(|_| NOT_SET_UP)?;
        let mut s = vault.service(SERVICE).ok_or(NOT_SET_UP)?;
        if s.usable().is_err() {
            crate::vault::wipe_str(&mut s.key);
            return Err(NOT_SET_UP.into());
        }
        Ok(s)
    }

    /// What Telnyx's error text is shown with: every vault value, the key among them, read
    /// for this one use and wiped after.
    fn scrubbed(&self, text: &str) -> String {
        let mut secrets = crate::vault::Vault::load(&self.vault)
            .map(|v| v.secrets())
            .unwrap_or_default();
        let shown = crate::settings::one_line(&crate::vault::scrub(text, &secrets), 200);
        secrets
            .iter_mut()
            .for_each(|(_, v)| crate::vault::wipe_str(v));
        shown
    }

    /// The key Telnyx signs with, read from the vault only when the file changed since the
    /// last time. `None` when there is no usable service.
    fn public_key(&self) -> Option<String> {
        let changed = std::fs::metadata(&self.vault)
            .and_then(|m| m.modified())
            .ok()?;
        let mut cached = self.public_key.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((at, key)) = cached.as_ref() {
            if *at == changed {
                return Some(key.clone());
            }
        }
        let key = self.service().ok()?.public_key;
        *cached = Some((changed, key.clone()));
        Some(key)
    }

    /// The caller id calls go out with.
    pub fn from(&self) -> Option<String> {
        self.service().ok().map(|s| s.from)
    }

    /// One Call Control request. The key is read from the vault for it and wiped once it is
    /// sent. An error is Telnyx's own words, cut, with every vault value scrubbed out of
    /// whatever it echoed.
    async fn post(&self, path: &[&str], body: Value) -> Result<Value, String> {
        let not_url = "the provider's address is not a URL";
        let mut url = url::Url::parse(&self.api).map_err(|_| not_url)?;
        // https, but for a test's stand-in on this machine: the key goes nowhere else.
        let local = matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"));
        if url.scheme() != "https" && !(url.scheme() == "http" && local) {
            return Err("not run: Telnyx's address must be https".into());
        }
        // A call id is Telnyx's text: as a path segment it cannot add one.
        (url.path_segments_mut()).map_err(|_| not_url)?.extend(path);
        let key = Key(std::mem::take(&mut self.read()?.key));
        // shortcut: reqwest copies the key into the request's header, which is dropped with the
        // request but not wiped; a wiping header needs a client of our own.
        let sent = self.http.post(url).bearer_auth(&key.0).json(&body).send();
        let res = sent.await;
        drop(key);
        let res = res.map_err(|e| {
            self.scrubbed(&format!("Telnyx could not be reached: {}", e.without_url()))
        })?;
        let status = res.status();
        let answer: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            let detail = &answer["errors"][0];
            let detail = (detail["detail"].as_str())
                .or(detail["title"].as_str())
                .unwrap_or_default();
            return Err(self.scrubbed(&format!("Telnyx answered {}: {detail}", status.as_u16())));
        }
        Ok(answer)
    }
}

/// The API key for one request, wiped when it is dropped.
struct Key(String);

impl Drop for Key {
    fn drop(&mut self) {
        crate::vault::wipe_str(&mut self.0);
    }
}

impl Telephony for Telnyx {
    async fn dial(&self, d: &Dial) -> Result<String, String> {
        let s = self.service()?;
        let mut body = json!({
            "connection_id": s.account,
            "to": d.to,
            "from": s.from,
            "timeout_secs": RING_SECS,
            "time_limit_secs": d.limit_secs.max(30),
            // ASSUMPTION(live): the call's events go to this address rather than the
            // connection's own webhook URL, and a POST is the default method.
            "webhook_url": d.events_url,
        });
        if let Some(stream_url) = &d.stream_url {
            body["stream_url"] = json!(stream_url);
            body["stream_track"] = json!("inbound_track");
            body["stream_bidirectional_mode"] = json!("rtp");
            body["stream_bidirectional_codec"] = json!("PCMU");
        }
        let answer = self.post(&["calls"], body).await?;
        (answer["data"]["call_control_id"].as_str())
            .filter(|id| !id.is_empty())
            .map(String::from)
            .ok_or_else(|| "Telnyx answered without a call id".into())
    }

    async fn hangup(&self, call: &str) -> Result<(), String> {
        let path = ["calls", call, "actions", "hangup"];
        self.post(&path, json!({})).await.map(|_| ())
    }

    async fn speak(&self, call: &str, text: &str, language: &str) -> Result<(), String> {
        let (voice, tag) = voice(language);
        let path = ["calls", call, "actions", "speak"];
        let body = json!({
            "payload": text,
            "payload_type": "text",
            "voice": voice,
            "language": tag,
        });
        self.post(&path, body).await.map(|_| ())
    }

    async fn transcribe(&self, call: &str, language: &str) -> Result<(), String> {
        let path = ["calls", call, "actions", "transcription_start"];
        let body = json!({
            "transcription_engine": "Telnyx",
            "transcription_engine_config": { "language": language },
            // ASSUMPTION(live): on a dialled call `inbound` is what the other party says, as
            // `inbound_track` is for a media stream.
            "transcription_tracks": "inbound",
        });
        self.post(&path, body).await.map(|_| ())
    }

    async fn dtmf(&self, call: &str, digit: char) -> Result<(), String> {
        let path = ["calls", call, "actions", "send_dtmf"];
        let body = json!({ "digits": digit.to_string() });
        self.post(&path, body).await.map(|_| ())
    }

    fn verify(&self, timestamp: &str, signature: &str, signed: &[u8], now: i64) -> bool {
        // Nothing is read for a request that carries no signature at all.
        if timestamp.is_empty() || signature.is_empty() {
            return false;
        }
        let Some(key) = self.public_key() else {
            return false;
        };
        verify(&key, timestamp, signature, signed, now)
    }
}

/// The signatures taken in the last `REMEMBER` seconds, by when they were: one is good once.
static SEEN: Mutex<Option<HashMap<String, i64>>> = Mutex::new(None);

/// Whether `signature` (base64) is `public_key`'s (base64, ed25519) over `{timestamp}|` and
/// `signed`, with a timestamp within `TOLERANCE` of `now`, and was not taken before. Anything
/// missing or malformed is a refusal.
pub fn verify(public_key: &str, timestamp: &str, signature: &str, signed: &[u8], now: i64) -> bool {
    let Ok(at) = timestamp.parse::<i64>() else {
        return false;
    };
    if (now - at).abs() > TOLERANCE {
        return false;
    }
    let (Ok(key), Ok(sig)) = (B64.decode(public_key.trim()), B64.decode(signature.trim())) else {
        return false;
    };
    let mut message = format!("{timestamp}|").into_bytes();
    message.extend_from_slice(signed);
    let key = ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, key);
    if key.verify(&message, &sig).is_err() {
        return false;
    }
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    let seen = seen.get_or_insert_with(HashMap::new);
    seen.retain(|_, at| now - *at <= REMEMBER);
    seen.insert(signature.trim().to_string(), at).is_none()
}

/// One frame of Telnyx's media socket.
#[derive(Debug, PartialEq, Eq)]
pub enum Frame {
    /// The stream began.
    Start,
    /// The other party's audio: μ-law, 8 kHz.
    /// ASSUMPTION(live): the payload is raw G.711 with no RTP header, as the docs say, in
    /// both directions.
    Media(Vec<u8>),
    /// The call ended.
    Stop,
    /// A mark sent after some audio came back: that audio has played.
    Mark(String),
    Other,
}

/// Reads one text frame. `None` for what is not JSON.
pub fn frame(text: &str) -> Option<Frame> {
    let v: Value = serde_json::from_str(text).ok()?;
    Some(match v["event"].as_str()? {
        "start" => Frame::Start,
        "stop" => Frame::Stop,
        "mark" => Frame::Mark(v["mark"]["name"].as_str().unwrap_or_default().into()),
        "media" => match v["media"]["payload"].as_str().map(|p| B64.decode(p)) {
            // Only what they said: with `inbound_track` nothing else is sent.
            Some(Ok(audio)) if v["media"]["track"].as_str() != Some("outbound") => {
                Frame::Media(audio)
            }
            _ => Frame::Other,
        },
        _ => Frame::Other,
    })
}

/// A frame of audio to play to the other party.
pub fn media(ulaw: &[u8]) -> String {
    json!({"event": "media", "media": {"payload": B64.encode(ulaw)}}).to_string()
}

/// Echoed by Telnyx once everything sent before it has played.
pub fn mark(name: &str) -> String {
    json!({"event": "mark", "mark": {"name": name}}).to_string()
}

/// Stops what is playing and drops what is queued.
pub const CLEAR: &str = r#"{"event":"clear"}"#;

/// One webhook: its event type (`call.answered`, `call.hangup`, `call.speak.ended`,
/// `call.transcription`), the call it is about, and for a transcription what was said.
#[derive(Debug, PartialEq, Eq)]
pub struct Hook {
    pub kind: String,
    pub call: String,
    /// A final transcript of something the other party said; `None` for an interim one.
    pub transcript: Option<String>,
}

/// Reads a webhook's body.
/// ASSUMPTION(live): the envelope is `data.event_type` and `data.payload.call_control_id`, as
/// Telnyx's webhook examples show, and a transcription carries
/// `data.payload.transcription_data.{transcript, is_final}` (the transcription page does not
/// print the payload).
pub fn event(body: &[u8]) -> Option<Hook> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let payload = &v["data"]["payload"];
    let said = &payload["transcription_data"];
    let transcript = (said["transcript"].as_str())
        .filter(|t| said["is_final"] != false && !t.trim().is_empty())
        .map(String::from);
    Some(Hook {
        kind: v["data"]["event_type"].as_str()?.to_string(),
        call: payload["call_control_id"].as_str()?.to_string(),
        transcript,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use ring::signature::KeyPair;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    pub struct Signer(ring::signature::Ed25519KeyPair);

    impl Signer {
        pub fn new() -> Signer {
            let rng = ring::rand::SystemRandom::new();
            let doc = ring::signature::Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
            Signer(ring::signature::Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap())
        }
        pub fn public(&self) -> String {
            B64.encode(self.0.public_key().as_ref())
        }
        pub fn sign(&self, timestamp: i64, signed: &str) -> String {
            B64.encode(self.0.sign(format!("{timestamp}|{signed}").as_bytes()))
        }
    }

    const URL: &str = "wss://box.example.ts.net/calls/tok";

    #[test]
    fn verify_takes_a_fresh_signature_once() {
        let telnyx = Signer::new();
        let now = 1_800_000_000;
        let sig = telnyx.sign(now - 2, URL);
        let stamp = (now - 2).to_string();
        assert!(verify(&telnyx.public(), &stamp, &sig, URL.as_bytes(), now));
        // The same connection again is a replay.
        assert!(!verify(&telnyx.public(), &stamp, &sig, URL.as_bytes(), now));
    }

    #[test]
    fn verify_refuses_unsigned_stale_other_key_and_other_url() {
        let (telnyx, other) = (Signer::new(), Signer::new());
        let now = 1_800_000_000;
        let stamp = now.to_string();
        let key = telnyx.public();
        let check =
            |stamp: &str, sig: &str, signed: &str| verify(&key, stamp, sig, signed.as_bytes(), now);
        assert!(!check(&stamp, "", URL), "unsigned");
        assert!(!check("", &telnyx.sign(now, URL), URL), "no timestamp");
        assert!(!check(&stamp, "not base64!", URL));
        assert!(!check(&stamp, &other.sign(now, URL), URL), "another key");
        let elsewhere = "wss://box.example.ts.net/calls/other";
        assert!(
            !check(&stamp, &telnyx.sign(now, elsewhere), URL),
            "another URL"
        );
        // Signed for another moment than the header says.
        assert!(!check(&stamp, &telnyx.sign(now - 1, URL), URL));
        for stale in [now - 301, now + 301] {
            let sig = telnyx.sign(stale, URL);
            assert!(!check(&stale.to_string(), &sig, URL), "{stale}");
        }
        let edge = telnyx.sign(now - 300, URL);
        assert!(check(&(now - 300).to_string(), &edge, URL));
        // A webhook is the same over its body.
        let body =
            r#"{"data":{"event_type":"call.answered","payload":{"call_control_id":"v3:a"}}}"#;
        assert!(check(&stamp, &telnyx.sign(now, body), body));
        let hook = event(body.as_bytes()).unwrap();
        assert_eq!(
            (hook.kind.as_str(), hook.call.as_str(), hook.transcript),
            ("call.answered", "v3:a", None)
        );
        // A transcription carries what was said, once it is final.
        let said = |is_final: bool| {
            format!(
                r#"{{"data":{{"event_type":"call.transcription","payload":{{"call_control_id":"v3:a","transcription_data":{{"transcript":"Seven is full","is_final":{is_final},"confidence":0.9}}}}}}}}"#
            )
        };
        assert_eq!(
            event(said(true).as_bytes()).unwrap().transcript.as_deref(),
            Some("Seven is full")
        );
        assert_eq!(event(said(false).as_bytes()).unwrap().transcript, None);
        assert!(
            !verify("", &stamp, &telnyx.sign(now, "x"), b"x", now),
            "no key"
        );
    }

    /// A one-request HTTP server answering `status` and `body`; gives the request it got.
    async fn stub(status: u16, body: &'static str) -> (String, tokio::task::JoinHandle<String>) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api = format!("http://{}/v2", listener.local_addr().unwrap());
        let served = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 4096];
            loop {
                let n = conn.read(&mut buf).await.unwrap();
                got.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&got).to_string();
                let Some((head, sent)) = text.split_once("\r\n\r\n") else {
                    continue;
                };
                let length = (head.lines())
                    .find_map(|l| {
                        l.to_lowercase()
                            .strip_prefix("content-length: ")?
                            .parse()
                            .ok()
                    })
                    .unwrap_or(0usize);
                if sent.len() >= length {
                    break;
                }
            }
            let answer = format!(
                "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            conn.write_all(answer.as_bytes()).await.unwrap();
            String::from_utf8_lossy(&got).to_string()
        });
        (api, served)
    }

    const KEY: &str = "KEY0123456789telnyx";

    /// A config whose vault has a `calls` service signed for by `public_key`, with the API at
    /// `api`.
    pub fn with_service(name: &str, api: &str, public_key: &str) -> Config {
        let mut cfg = crate::calls::tests::config(name, "");
        let mut vault = crate::vault::Vault::parse("").unwrap();
        let service = Service {
            name: SERVICE.into(),
            provider: "telnyx".into(),
            key: KEY.into(),
            account: "1494404757140276705".into(),
            from: "+14165550100".into(),
            public_key: public_key.into(),
        };
        vault.put_service(service).unwrap();
        std::fs::write(crate::vault::path(&cfg), vault.to_string()).unwrap();
        cfg.calls_api = api.into();
        cfg
    }

    #[tokio::test]
    async fn dial_sends_the_stream_and_never_asks_to_record() {
        let (api, served) = stub(
            200,
            r#"{"data":{"call_control_id":"v3:abc","is_alive":false}}"#,
        )
        .await;
        let cfg = with_service("telnyx-dial", &api, crate::vault::tests::TEST_PUBLIC_KEY);
        let telnyx = Telnyx::load(&cfg).unwrap();
        let dial = Dial {
            to: "+14165550123".into(),
            stream_url: Some(URL.into()),
            events_url: "https://box.example.ts.net/calls/events".into(),
            limit_secs: 330,
        };
        assert_eq!(telnyx.dial(&dial).await.unwrap(), "v3:abc");
        let request = served.await.unwrap();
        assert!(request.starts_with("POST /v2/calls HTTP/1.1"), "{request}");
        assert!(request
            .to_lowercase()
            .contains(&format!("authorization: bearer {}", KEY.to_lowercase())));
        let body: Value = serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(
            body,
            json!({
                "connection_id": "1494404757140276705",
                "to": "+14165550123",
                "from": "+14165550100",
                "timeout_secs": 30,
                "time_limit_secs": 330,
                "stream_url": URL,
                "stream_track": "inbound_track",
                "stream_bidirectional_mode": "rtp",
                "stream_bidirectional_codec": "PCMU",
                "webhook_url": "https://box.example.ts.net/calls/events",
            })
        );
        assert!(body.get("record").is_none());
    }

    #[tokio::test]
    async fn a_provider_error_never_quotes_the_key() {
        let (api, served) = stub(
            401,
            r#"{"errors":[{"code":"10009","title":"Authentication failed","detail":"The key KEY0123456789telnyx is not valid"}]}"#,
        )
        .await;
        let cfg = with_service("telnyx-error", &api, crate::vault::tests::TEST_PUBLIC_KEY);
        let telnyx = Telnyx::load(&cfg).unwrap();
        let err = telnyx.hangup("v3:a/../b").await.unwrap_err();
        assert_eq!(
            err,
            "Telnyx answered 401: The key [vault:calls] is not valid"
        );
        // The id stayed one path segment.
        let request = served.await.unwrap();
        assert!(
            request.starts_with("POST /v2/calls/v3:a%2F..%2Fb/actions/hangup HTTP/1.1"),
            "{request}"
        );
        // No service, or half of one: nothing is asked.
        std::fs::write(crate::vault::path(&cfg), "").unwrap();
        assert_eq!(telnyx.hangup("x").await.unwrap_err(), NOT_SET_UP);
        assert!(Telnyx::load(&cfg).is_err());
        assert!(!format!("{:?}", Telnyx::load(&cfg).err()).contains(KEY));
    }

    /// The API base is the config's, never the vault's, and is https but for a test's
    /// stand-in on this machine: the key goes to nothing else.
    #[tokio::test]
    async fn the_api_base_is_https_or_a_local_stand_in() {
        let plain = crate::calls::tests::config("telnyx-https-default", "");
        assert_eq!(plain.calls_api, API, "Telnyx's own when nothing sets it");
        let mut cfg = with_service("telnyx-https", "", crate::vault::tests::TEST_PUBLIC_KEY);
        cfg.calls_api = "http://api.telnyx.com/v2".into();
        let err = Telnyx::load(&cfg).unwrap().hangup("x").await.unwrap_err();
        assert_eq!(err, "not run: Telnyx's address must be https");
        // The vault's old `api` key is read by nothing.
        let path = crate::vault::path(&cfg);
        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, format!("{text}api = \"http://127.0.0.1:9/v2\"\n")).unwrap();
        cfg.calls_api = API.into();
        assert_eq!(Telnyx::new(&cfg).api, API);
    }

    /// With Telnyx's own speech the dial asks for no stream, and speaking and transcribing
    /// are two more Call Control requests that carry only text and a language.
    #[tokio::test]
    async fn provider_speech_dials_without_a_stream_and_speaks_by_request() {
        let body_of = |request: String| -> Value {
            serde_json::from_str(request.split_once("\r\n\r\n").unwrap().1).unwrap()
        };
        let (api, served) = stub(200, r#"{"data":{"call_control_id":"v3:abc"}}"#).await;
        let cfg = with_service(
            "telnyx-speech-dial",
            &api,
            crate::vault::tests::TEST_PUBLIC_KEY,
        );
        let dial = Dial {
            to: "+15145550123".into(),
            stream_url: None,
            events_url: "https://box.example.ts.net/calls/events".into(),
            limit_secs: 330,
        };
        Telnyx::load(&cfg).unwrap().dial(&dial).await.unwrap();
        let body = body_of(served.await.unwrap());
        assert!(body.get("stream_url").is_none() && body.get("stream_track").is_none());
        assert!(body.get("record").is_none());
        assert_eq!(
            body["webhook_url"],
            "https://box.example.ts.net/calls/events"
        );

        let (api, served) = stub(200, r#"{"data":{"result":"ok"}}"#).await;
        let cfg = with_service(
            "telnyx-speech-speak",
            &api,
            crate::vault::tests::TEST_PUBLIC_KEY,
        );
        let telnyx = Telnyx::load(&cfg).unwrap();
        telnyx.speak("v3:abc", "Bonjour.", "fr").await.unwrap();
        let request = served.await.unwrap();
        assert!(request.starts_with("POST /v2/calls/v3:abc/actions/speak HTTP/1.1"));
        assert_eq!(
            body_of(request),
            json!({"payload": "Bonjour.", "payload_type": "text",
                   "voice": "AWS.Polly.Gabrielle-Neural", "language": "fr-CA"})
        );
        assert_eq!(voice("en"), ("AWS.Polly.Joanna-Neural", "en-US"));
        assert_eq!(voice("ko"), ("AWS.Polly.Seoyeon-Neural", "ko-KR"));

        let (api, served) = stub(200, r#"{"data":{"result":"ok"}}"#).await;
        let cfg = with_service(
            "telnyx-speech-hear",
            &api,
            crate::vault::tests::TEST_PUBLIC_KEY,
        );
        let telnyx = Telnyx::load(&cfg).unwrap();
        telnyx.transcribe("v3:abc", "ko").await.unwrap();
        let request = served.await.unwrap();
        assert!(request.starts_with("POST /v2/calls/v3:abc/actions/transcription_start HTTP/1.1"));
        assert_eq!(
            body_of(request),
            json!({"transcription_engine": "Telnyx",
                   "transcription_engine_config": {"language": "ko"},
                   "transcription_tracks": "inbound"})
        );
    }

    /// The listener's check reads the vault only when the file changed (review finding 10).
    #[test]
    fn the_public_key_is_read_again_only_when_the_vault_changes() {
        let (first, second) = (Signer::new(), Signer::new());
        let cfg = with_service("telnyx-key-cache", "", &first.public());
        let telnyx = Telnyx::new(&cfg);
        let now = 1_800_000_100;
        let check =
            |by: &Signer, at: i64| telnyx.verify(&at.to_string(), &by.sign(at, "x"), b"x", now);
        assert!(check(&first, now));
        // No signature: refused before anything is read.
        assert!(!telnyx.verify("", "", b"x", now));
        // The owner saves another key: the file changed, so it is read again.
        let mut vault = crate::vault::Vault::load(&crate::vault::path(&cfg)).unwrap();
        let mut service = vault.service(SERVICE).unwrap();
        service.public_key = second.public();
        vault.put_service(service).unwrap();
        let path = crate::vault::path(&cfg);
        std::fs::write(&path, vault.to_string()).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options()
            .write(true)
            .open(&path)
            .unwrap()
            .set_modified(later)
            .unwrap();
        assert!(check(&second, now - 1));
        assert!(!check(&first, now - 2), "the old key is no longer Telnyx's");
        // The vault gone: nothing verifies.
        std::fs::remove_file(&path).unwrap();
        assert!(!check(&second, now - 3));
    }

    #[test]
    fn frames_decode_and_encode() {
        assert_eq!(
            frame(r#"{"event":"connected","version":"1.0.0"}"#),
            Some(Frame::Other)
        );
        assert_eq!(frame(r#"{"event":"start","start":{}}"#), Some(Frame::Start));
        let audio = r#"{"event":"media","media":{"track":"inbound","chunk":"1","payload":"//8A"}}"#;
        assert_eq!(frame(audio), Some(Frame::Media(vec![0xff, 0xff, 0x00])));
        assert_eq!(
            frame(r#"{"event":"media","media":{"payload":"not base64"}}"#),
            Some(Frame::Other)
        );
        assert_eq!(frame(r#"{"event":"stop"}"#), Some(Frame::Stop));
        assert_eq!(
            frame(r#"{"event":"mark","mark":{"name":"s3"}}"#),
            Some(Frame::Mark("s3".into()))
        );
        assert_eq!(frame("not json"), None);
        assert_eq!(
            frame(&media(&[0xff, 0xff, 0x00])),
            Some(Frame::Media(vec![0xff, 0xff, 0x00]))
        );
        assert_eq!(frame(&mark("s3")), Some(Frame::Mark("s3".into())));
        assert_eq!(frame(CLEAR), Some(Frame::Other));
    }
}
