//! Credentials: exchange a Cursor API key for the short-lived bearer token the agent service takes,
//! and refresh it before it expires.
//!
//! The key itself arrives from rpi (`SimpleStreamOptions::api_key`, which rpi resolves from the
//! account's auth store) or from `CURSOR_API_KEY`. Nothing here logs, stores or formats a token:
//! the value lives in this process's memory and dies with the rpi child.
use sha2::Digest;
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Refresh this long before the token's real expiry, so a turn never starts on a token that dies
/// mid-stream.
pub const REFRESH_MARGIN: Duration = Duration::from_secs(120);

/// Bound on one credential request: a hung exchange must not stall a turn forever.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);

/// The credential host; `CURSOR_API_BASE` overrides it (tests point this at a loopback server).
const DEFAULT_BASE: &str = "https://api2.cursor.sh";
/// Cursor user API keys (`crsr_…`, minted at cursor.com/dashboard/api) are not bearer credentials
/// themselves: they are exchanged once and the result is reused (attribution: `omni-cursor-auth.ts`).
const API_KEY_PREFIX: &str = "crsr_";
const EXCHANGE_PATH: &str = "/auth/exchange_user_api_key";
const REFRESH_PATH: &str = "/auth/refresh";
/// A token whose JWT carries no readable `exp` is treated as fresh for this long, matching the
/// one-hour session Cursor issues (attribution: `omni-cursor-auth.ts`, `FALLBACK_TTL_MS`).
const ASSUMED_TTL: Duration = Duration::from_secs(50 * 60);

/// A bearer token and the moment it stops being usable.
#[derive(Clone)]
pub struct Token {
    value: String,
    expires: Instant,
}

impl Token {
    pub fn new(value: String, ttl: Duration) -> Self {
        Self {
            value,
            expires: Instant::now() + ttl,
        }
    }

    /// Never printed: `Debug` shows only the TTL.
    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn expired(&self) -> bool {
        Instant::now() + REFRESH_MARGIN >= self.expires
    }
}

/// Never printed: only the fact that a token exists, and when it stops being usable.
impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Token(<redacted>)")
    }
}

/// One API key's live credential pair. Keyed in [`CACHE`] by the SHA-256 of the key the pair came
/// from, so the raw key is never a map key (nor a `Debug` line).
struct Session {
    token: Token,
    /// `None` when the service sent no refresh token: the only way back is re-exchanging the key.
    refresh: Option<String>,
}

static CACHE: OnceLock<Mutex<HashMap<[u8; 32], Session>>> = OnceLock::new();

fn cache() -> &'static Mutex<HashMap<[u8; 32], Session>> {
    CACHE.get_or_init(Mutex::default)
}

fn fingerprint(key: &str) -> [u8; 32] {
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&sha2::Sha256::digest(key.as_bytes()));
    digest
}

/// The bearer token for this turn: `explicit` (rpi's resolved key) when given, `CURSOR_API_KEY`
/// otherwise. API keys (`crsr_…`) are exchanged; anything else is taken as a bearer token already.
pub async fn token(explicit: Option<&str>) -> anyhow::Result<String> {
    let key = credential(explicit)?;
    if key.starts_with(API_KEY_PREFIX) {
        return api_key_token(&key).await;
    }
    // A three-segment value is a JWT — a bearer token already — and any other shape is passed
    // through unchanged: the service rejects what it does not accept with its own error, and a
    // request of our own would only add a second way for the turn to fail.
    Ok(key)
}

/// The credential for this turn. The error names both places a key can come from and no value.
fn credential(explicit: Option<&str>) -> anyhow::Result<String> {
    let explicit = explicit.filter(|value| !value.trim().is_empty());
    let stored = std::env::var("CURSOR_API_KEY")
        .ok()
        .filter(|value| !value.trim().is_empty());
    match explicit.map(str::to_string).or(stored) {
        Some(key) => Ok(key),
        None => anyhow::bail!(
            "no Cursor credential: the account has no API key in rpi's auth store and \
             CURSOR_API_KEY is unset"
        ),
    }
}

/// The bearer token for an API key: the cached session while it is usable, a refresh when it is
/// about to expire, and a fresh exchange otherwise.
async fn api_key_token(key: &str) -> anyhow::Result<String> {
    let fingerprint = fingerprint(key);
    let base = base()?;

    let mut refresh = None;
    {
        let cache = cache();
        let cache = cache
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(session) = cache.get(&fingerprint) {
            if !session.token.expired() {
                return Ok(session.token.value().to_string());
            }
            refresh = session.refresh.clone();
        }
    }

    let session = match refresh {
        Some(refresh) => match refreshed(&base, &refresh).await {
            Ok(session) => session,
            // A refresh token the service no longer honours is not fatal: the key itself always
            // mints a new pair.
            Err(_) => exchanged(&base, key).await?,
        },
        None => exchanged(&base, key).await?,
    };
    let value = session.token.value().to_string();
    let cache = cache();
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .insert(fingerprint, session);
    Ok(value)
}

async fn exchanged(base: &reqwest::Url, key: &str) -> anyhow::Result<Session> {
    let tokens = request(base, EXCHANGE_PATH, key, "API key exchange").await?;
    Ok(tokens.into_session())
}

async fn refreshed(base: &reqwest::Url, refresh_token: &str) -> anyhow::Result<Session> {
    let tokens = request(base, REFRESH_PATH, refresh_token, "token refresh").await?;
    Ok(tokens.into_session())
}

/// `{"accessToken": "...", "refreshToken": "..."}`, the shape both credential endpoints answer
/// with (attribution: `ullage-api.rs`, `ExchangeTokens`).
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct TokensBody {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
}

impl TokensBody {
    fn into_session(self) -> Session {
        let ttl = jwt_ttl(&self.access_token).unwrap_or(ASSUMED_TTL);
        Session {
            token: Token::new(self.access_token, ttl),
            refresh: self.refresh_token.filter(|value| !value.is_empty()),
        }
    }
}

/// One credential request. `label` names the operation in errors; no error carries the credential,
/// the response body or a token.
async fn request(
    base: &reqwest::Url,
    path: &str,
    bearer: &str,
    label: &str,
) -> anyhow::Result<TokensBody> {
    let response = client()?
        .post(endpoint(base, path))
        .bearer_auth(bearer)
        .json(&serde_json::json!({}))
        .send()
        .await
        .map_err(|err| {
            let reason = if err.is_timeout() {
                "timed out"
            } else if err.is_connect() {
                "could not connect"
            } else {
                "failed"
            };
            anyhow::anyhow!("Cursor {label} request {reason}")
        })?;

    let status = response.status();
    if !status.is_success() {
        anyhow::bail!("Cursor {label} failed with HTTP {status}");
    }
    let body = response.json::<TokensBody>().await.map_err(|_| {
        anyhow::anyhow!("Cursor {label} answered with a body that is not the expected JSON")
    })?;
    if body.access_token.trim().is_empty() {
        anyhow::bail!("Cursor {label} returned an empty access token");
    }
    Ok(body)
}

/// The credential host, refusing anything that would put a bearer token on the wire in clear: a
/// non-https base is only accepted when it is a loopback address (the test server).
fn base() -> anyhow::Result<reqwest::Url> {
    let raw = std::env::var("CURSOR_API_BASE").unwrap_or_else(|_| DEFAULT_BASE.to_string());
    let url = reqwest::Url::parse(raw.trim())
        .map_err(|_| anyhow::anyhow!("CURSOR_API_BASE is not a valid URL"))?;
    if !is_secure_or_loopback(&url) {
        anyhow::bail!("CURSOR_API_BASE must be https or a loopback address");
    }
    Ok(url)
}

fn endpoint(base: &reqwest::Url, path: &str) -> String {
    format!("{}{path}", base.as_str().trim_end_matches('/'))
}

fn client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        // Redirects are refused outright: a bearer token must never be followed to a host the
        // server picked (attribution: `ullage-api.rs`).
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .map_err(|_| anyhow::anyhow!("failed to build the Cursor credential HTTP client"))
}

/// Attribution: `ullage-api.rs` (`is_secure_or_loopback`).
fn is_secure_or_loopback(url: &reqwest::Url) -> bool {
    let secure = url.scheme() == "https" && url.host().is_some();
    let loopback = url.scheme() == "http"
        && url.host_str().is_some_and(|host| {
            let host = host.trim_start_matches('[').trim_end_matches(']');
            host == "localhost"
                || host
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|ip| ip.is_loopback())
        });
    secure || loopback
}

/// How long the token stays usable, read from the JWT payload's `exp`. The signature is not
/// checked — the service is the authority on that; we only avoid sending a token it is about to
/// refuse. `None` when the value is not a JWT or carries no numeric `exp`.
fn jwt_ttl(value: &str) -> Option<Duration> {
    let payload = value.split('.').nth(1)?;
    let decoded = base64::Engine::decode(
        &base64::engine::general_purpose::URL_SAFE_NO_PAD,
        payload.trim_end_matches('='),
    )
    .ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&decoded).ok()?;
    let exp = claims.get("exp")?.as_u64()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH).ok()?.as_secs();
    Some(Duration::from_secs(exp.saturating_sub(now)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use serde_json::json;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::{TcpListener, TcpStream};

    const TEST_TIMEOUT: Duration = Duration::from_secs(20);

    /// One request the test server answered.
    struct Hit {
        path: String,
        bearer: String,
    }

    /// A plaintext HTTP/1.1 server on `127.0.0.1:0` that answers by request path, and records what
    /// it was asked. Hand-rolled because the assertions are about which endpoints `token()` calls
    /// with which bearer, and how many times.
    struct Api {
        base: String,
        hits: Arc<std::sync::Mutex<Vec<Hit>>>,
    }

    impl Api {
        fn answers(&self) -> std::sync::MutexGuard<'_, Vec<Hit>> {
            self.hits
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        }
    }

    async fn start_api<F>(answer: F) -> Api
    where
        F: Fn(&str, &str) -> (u16, String) + Send + Sync + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a loopback listener");
        let address = listener.local_addr().expect("listener address");
        let hits = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = hits.clone();
        let answer = Arc::new(answer);
        tokio::spawn(async move {
            while let Ok((mut socket, _)) = listener.accept().await {
                let recorded = recorded.clone();
                let answer = answer.clone();
                tokio::spawn(async move {
                    let Some((path, bearer)) = read_request(&mut socket).await else {
                        return;
                    };
                    let (status, body) = answer(&path, &bearer);
                    recorded
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(Hit {
                            path: path.clone(),
                            bearer,
                        });
                    write_response(&mut socket, status, &body).await;
                });
            }
        });
        Api {
            base: format!("http://{address}"),
            hits,
        }
    }

    /// Reads one request head and its body, returning `(path, bearer)`.
    async fn read_request(socket: &mut TcpStream) -> Option<(String, String)> {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        let head_end = loop {
            if let Some(index) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                break index + 4;
            }
            let read = socket.read(&mut chunk).await.ok()?;
            if read == 0 {
                return None;
            }
            buffer.extend_from_slice(&chunk[..read]);
        };

        let head = String::from_utf8_lossy(&buffer[..head_end]).to_string();
        let mut path = String::new();
        let mut bearer = String::new();
        let mut body_len = 0usize;
        for (index, line) in head.split("\r\n").enumerate() {
            if index == 0 {
                path = line
                    .split_whitespace()
                    .nth(1)
                    .unwrap_or_default()
                    .to_string();
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            if name.eq_ignore_ascii_case("authorization") {
                bearer = value.strip_prefix("Bearer ").unwrap_or(value).to_string();
            } else if name.eq_ignore_ascii_case("content-length") {
                body_len = value.parse().unwrap_or(0);
            }
        }

        let mut unread = body_len.saturating_sub(buffer.len() - head_end);
        while unread > 0 {
            let want = unread.min(chunk.len());
            let read = socket.read(&mut chunk[..want]).await.ok()?;
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            unread -= read;
        }
        Some((path, bearer))
    }

    async fn write_response(socket: &mut TcpStream, status: u16, body: &str) {
        let response = format!(
            "HTTP/1.1 {status} {}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
             connection: close\r\n\r\n{body}",
            if status == 200 {
                "OK"
            } else {
                "Internal Server Error"
            },
            body.len()
        );
        let _ = socket.write_all(response.as_bytes()).await;
        let _ = socket.flush().await;
        let _ = socket.shutdown().await;
    }

    /// A JWT-shaped value with `exp` this many seconds from now; `label` only makes two tokens
    /// distinguishable when a test compares them.
    fn jwt(expires_in: u64, label: &str) -> String {
        let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
        let exp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("a clock after the epoch")
            .as_secs()
            + expires_in;
        format!(
            "{}.{}.signature",
            engine.encode(br#"{"alg":"none","typ":"JWT"}"#),
            engine.encode(format!(r#"{{"exp":{exp},"sub":"{label}"}}"#)),
        )
    }

    fn token_body(access: &str, refresh: &str) -> String {
        json!({ "accessToken": access, "refreshToken": refresh }).to_string()
    }

    /// The tokens the [`scripted_api`] service hands out, so assertions compare against the exact
    /// values the server produced.
    struct Script {
        /// The token an ordinary key is exchanged for.
        exchange: String,
        /// A token that is already inside [`REFRESH_MARGIN`] when it arrives.
        short: String,
        refresh: String,
        refreshed: String,
    }

    /// A credential service that behaves like the real one: `crsr_…` keys are exchanged, a key
    /// whose value contains `short` gets a nearly expired access token, and `refresh` rotates the
    /// pair.
    async fn scripted_api() -> (Api, Arc<Script>) {
        let script = Arc::new(Script {
            exchange: jwt(3600, "access-exchange"),
            short: jwt(30, "access-short"),
            refresh: jwt(3600, "refresh-exchange"),
            refreshed: jwt(3600, "access-refreshed"),
        });
        let answers = script.clone();
        let api = start_api(move |path, bearer| match path {
            EXCHANGE_PATH if bearer.contains("short") => {
                (200, token_body(&answers.short, &answers.refresh))
            }
            EXCHANGE_PATH => (200, token_body(&answers.exchange, &answers.refresh)),
            REFRESH_PATH => (200, token_body(&answers.refreshed, &answers.refresh)),
            _ => (404, "{}".to_string()),
        })
        .await;
        (api, script)
    }

    /// `CURSOR_API_BASE` and `CURSOR_API_KEY` are process-global, so the tests that touch them take
    /// turns; the guard puts both back, even on panic.
    static ENVIRONMENT: OnceLock<tokio::sync::Mutex<()>> = OnceLock::new();

    struct Environment {
        _turn: tokio::sync::MutexGuard<'static, ()>,
        previous_base: Option<String>,
        previous_key: Option<String>,
    }

    impl Drop for Environment {
        fn drop(&mut self) {
            restore("CURSOR_API_BASE", self.previous_base.take());
            restore("CURSOR_API_KEY", self.previous_key.take());
        }
    }

    fn restore(name: &str, value: Option<String>) {
        match value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }

    /// Points the credential base at `base` and hides any ambient API key for the test's duration.
    async fn point_at(base: &str) -> Environment {
        let turn = ENVIRONMENT
            .get_or_init(|| tokio::sync::Mutex::new(()))
            .lock()
            .await;
        let environment = Environment {
            _turn: turn,
            previous_base: std::env::var("CURSOR_API_BASE").ok(),
            previous_key: std::env::var("CURSOR_API_KEY").ok(),
        };
        std::env::set_var("CURSOR_API_BASE", base);
        std::env::remove_var("CURSOR_API_KEY");
        environment
    }

    #[tokio::test]
    async fn an_api_key_is_exchanged_for_a_bearer_token() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (api, script) = scripted_api().await;
            let _environment = point_at(&api.base).await;

            let token = token(Some("crsr_test_exchange")).await.expect("a token");

            assert_eq!(token, script.exchange);
            let hits = api.answers();
            assert_eq!(hits.len(), 1);
            assert_eq!(hits[0].path, EXCHANGE_PATH);
            assert_eq!(hits[0].bearer, "crsr_test_exchange");
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn a_second_turn_reuses_the_cached_session() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (api, script) = scripted_api().await;
            let _environment = point_at(&api.base).await;

            let first = token(Some("crsr_test_cached")).await.expect("a token");
            let second = token(Some("crsr_test_cached")).await.expect("a token");

            assert_eq!(first, script.exchange);
            assert_eq!(second, script.exchange);
            // One exchange for the whole process, not one per turn.
            assert_eq!(api.answers().len(), 1);
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn a_token_inside_the_refresh_margin_is_refreshed() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (api, script) = scripted_api().await;
            let _environment = point_at(&api.base).await;

            let first = token(Some("crsr_test_short")).await.expect("a token");
            let second = token(Some("crsr_test_short")).await.expect("a token");

            assert_eq!(first, script.short);
            assert_eq!(second, script.refreshed);
            {
                // Scoped: the std Mutex is locked again below, and held across it would deadlock.
                let hits = api.answers();
                assert_eq!(hits.len(), 2);
                assert_eq!(hits[1].path, REFRESH_PATH);
                assert_eq!(hits[1].bearer, script.refresh);
            }

            // The rotated pair replaced the cached one, so the next turn needs no request.
            let third = token(Some("crsr_test_short")).await.expect("a token");
            assert_eq!(third, script.refreshed);
            assert_eq!(api.answers().len(), 2);
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn a_jwt_is_used_as_a_bearer_token_without_a_request() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (api, _script) = scripted_api().await;
            let _environment = point_at(&api.base).await;

            let value = jwt(3600, "explicit");
            let token = token(Some(&value)).await.expect("a token");

            assert_eq!(token, value);
            assert!(api.answers().is_empty());
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn the_environment_key_is_used_when_rpi_resolves_none() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (api, script) = scripted_api().await;
            let _environment = point_at(&api.base).await;
            std::env::set_var("CURSOR_API_KEY", "crsr_test_from_env");

            let token = token(None).await.expect("a token");

            assert_eq!(token, script.exchange);
            assert_eq!(api.answers()[0].bearer, "crsr_test_from_env");
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn a_missing_credential_names_both_places_and_no_value() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let (api, _script) = scripted_api().await;
            let _environment = point_at(&api.base).await;

            let error = token(None).await.expect_err("no credential");

            let message = format!("{error:#}");
            assert!(message.contains("CURSOR_API_KEY"), "{message}");
            assert!(message.contains("auth store"), "{message}");
            assert!(api.answers().is_empty());
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn a_failed_exchange_reports_neither_the_key_nor_a_token() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let api = start_api(|_, _| (500, "{}".to_string())).await;
            let _environment = point_at(&api.base).await;
            let key = "crsr_test_rejected_500";

            let error = token(Some(key))
                .await
                .expect_err("a 500 fails the exchange");
            let message = format!("{error:#}");

            assert!(message.contains("500"), "{message}");
            assert!(!message.contains(key), "{message}");
            assert!(!message.contains("accessToken"), "{message}");
            // `Token`'s `Debug` is the only way a token could reach a log line.
            let redacted = format!(
                "{:?}",
                Token::new("secret-access-value".to_string(), Duration::from_secs(60))
            );
            assert!(!redacted.contains("secret-access-value"), "{redacted}");
        })
        .await
        .expect("bounded");
    }

    #[tokio::test]
    async fn a_plaintext_remote_base_is_refused() {
        tokio::time::timeout(TEST_TIMEOUT, async {
            let _environment = point_at("http://api2.cursor.sh.invalid").await;

            let error = token(Some("crsr_test_plaintext"))
                .await
                .expect_err("refused");

            let message = format!("{error:#}");
            assert!(message.contains("CURSOR_API_BASE"), "{message}");
            assert!(message.contains("https"), "{message}");
        })
        .await
        .expect("bounded");
    }

    #[test]
    fn a_jwt_payload_yields_its_remaining_lifetime() {
        assert!(jwt_ttl(&jwt(600, "lifetime")).expect("an exp") <= Duration::from_secs(600));
        assert!(jwt_ttl("not-a-jwt").is_none());
        assert!(jwt_ttl(&format!("header.{}.signature", {
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(br#"{"sub":"no-exp"}"#)
        }))
        .is_none());
    }
}
