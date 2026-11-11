//! Telnyx, the one telephony provider built (DESIGN §4.22): Call Control over the daemon's own
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
const API: &str = "https://api.telnyx.com/v2";
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
    /// `wss://host/calls/<token>`: exactly what Telnyx signs.
    pub stream_url: String,
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
    http: reqwest::Client,
}

/// Why a call cannot be placed at all.
pub const NOT_SET_UP: &str = "not run: the call provider is not set up (zephyr vault add \
                              service calls on the host)";

impl Telnyx {
    /// The client, once the vault has a usable `calls` service.
    pub fn load(cfg: &Config) -> Result<Telnyx, String> {
        let telnyx = Telnyx {
            vault: crate::vault::path(cfg),
            http: reqwest::Client::builder()
                .timeout(HTTP_WAIT)
                .build()
                .map_err(|e| format!("the HTTP client could not start: {e}"))?,
        };
        telnyx.service().map(|_| telnyx)
    }

    fn service(&self) -> Result<Service, String> {
        let vault = crate::vault::Vault::load(&self.vault).map_err(|_| NOT_SET_UP)?;
        let s = vault.service(SERVICE).ok_or(NOT_SET_UP)?;
        let whole = s.provider == "telnyx"
            && !s.key.is_empty()
            && !s.account.is_empty()
            && s.from.starts_with('+')
            && !s.public_key.is_empty();
        whole.then_some(s).ok_or_else(|| NOT_SET_UP.into())
    }

    /// The caller id calls go out with.
    pub fn from(&self) -> Option<String> {
        self.service().ok().map(|s| s.from)
    }

    /// One Call Control request. An error is Telnyx's own words, cut, with the key scrubbed
    /// out of whatever it echoed.
    async fn post(&self, path: &[&str], body: Value) -> Result<Value, String> {
        let s = self.service()?;
        let base = if s.api.is_empty() { API } else { &s.api };
        let mut url = url::Url::parse(base).map_err(|_| "the provider's address is not a URL")?;
        // A call id is Telnyx's text: as a path segment it cannot add one.
        (url.path_segments_mut())
            .map_err(|_| "the provider's address is not a URL")?
            .extend(path);
        let scrub = |text: String| {
            let secrets = [(SERVICE.to_string(), s.key.clone())];
            crate::settings::one_line(&crate::vault::scrub(&text, &secrets), 200)
        };
        let sent = self.http.post(url).bearer_auth(&s.key).json(&body).send();
        let res = sent
            .await
            .map_err(|e| scrub(format!("Telnyx could not be reached: {}", e.without_url())))?;
        let status = res.status();
        let answer: Value = res.json().await.unwrap_or_default();
        if !status.is_success() {
            let detail = &answer["errors"][0];
            let detail = (detail["detail"].as_str())
                .or(detail["title"].as_str())
                .unwrap_or_default();
            return Err(scrub(format!(
                "Telnyx answered {}: {detail}",
                status.as_u16()
            )));
        }
        Ok(answer)
    }
}

impl Telephony for Telnyx {
    async fn dial(&self, d: &Dial) -> Result<String, String> {
        let s = self.service()?;
        let body = json!({
            "connection_id": s.account,
            "to": d.to,
            "from": s.from,
            "timeout_secs": RING_SECS,
            "time_limit_secs": d.limit_secs.max(30),
            "stream_url": d.stream_url,
            "stream_track": "inbound_track",
            "stream_bidirectional_mode": "rtp",
            "stream_bidirectional_codec": "PCMU",
            // ASSUMPTION(live): the call's events go to this address rather than the
            // connection's own webhook URL, and a POST is the default method.
            "webhook_url": d.events_url,
        });
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

    async fn dtmf(&self, call: &str, digit: char) -> Result<(), String> {
        let path = ["calls", call, "actions", "send_dtmf"];
        let body = json!({ "digits": digit.to_string() });
        self.post(&path, body).await.map(|_| ())
    }

    fn verify(&self, timestamp: &str, signature: &str, signed: &[u8], now: i64) -> bool {
        let Ok(s) = self.service() else {
            return false;
        };
        verify(&s.public_key, timestamp, signature, signed, now)
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

/// A webhook's `(event type, call id)`: `call.answered`, `call.hangup`.
/// ASSUMPTION(live): the envelope is `data.event_type` and `data.payload.call_control_id`, as
/// Telnyx's webhook examples show.
pub fn event(body: &[u8]) -> Option<(String, String)> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let kind = v["data"]["event_type"].as_str()?;
    let call = v["data"]["payload"]["call_control_id"].as_str()?;
    Some((kind.to_string(), call.to_string()))
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
        assert_eq!(
            event(body.as_bytes()),
            Some(("call.answered".into(), "v3:a".into()))
        );
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

    /// A config whose vault has a `calls` service on `api`, signed for by `public_key`.
    pub fn with_service(name: &str, api: &str, public_key: &str) -> Config {
        let cfg = crate::calls::tests::config(name, "");
        let mut vault = crate::vault::Vault::parse("").unwrap();
        let service = Service {
            name: SERVICE.into(),
            provider: "telnyx".into(),
            key: KEY.into(),
            account: "1494404757140276705".into(),
            from: "+14165550100".into(),
            public_key: public_key.into(),
            api: api.into(),
        };
        vault.put_service(service).unwrap();
        std::fs::write(crate::vault::path(&cfg), vault.to_string()).unwrap();
        cfg
    }

    #[tokio::test]
    async fn dial_sends_the_stream_and_never_asks_to_record() {
        let (api, served) = stub(
            200,
            r#"{"data":{"call_control_id":"v3:abc","is_alive":false}}"#,
        )
        .await;
        let cfg = with_service("telnyx-dial", &api, "cHVi");
        let telnyx = Telnyx::load(&cfg).unwrap();
        let dial = Dial {
            to: "+14165550123".into(),
            stream_url: URL.into(),
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
        let cfg = with_service("telnyx-error", &api, "cHVi");
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
