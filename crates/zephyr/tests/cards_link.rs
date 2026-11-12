//! The Link issuer against a stub of `api.link.com` and `login.link.com` (DESIGN §4.25), and
//! the ignored live test the owner runs with their own Link account.

use http_body_util::{BodyExt, Full};
use hyper::body::{Bytes, Incoming};
use hyper::{Request, Response};
use serde_json::{json, Value};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use zephyr::cards::{CardSpec, CardState, Issuer};
use zephyr::cards_link::Link;
use zephyr::config::Config;
use zephyr::store::Card;

/// What the stub Link holds: every call, the one spend request, and the tokens it hands out.
#[derive(Default)]
struct Fake {
    /// `(method, path with query, bearer token, body)`; a form body is its text.
    calls: Vec<(String, String, String, Value)>,
    /// The spend request's `status`.
    status: String,
    /// What a retrieve adds to the request (`payment_status_details`, a card without a code).
    extra: Value,
    refreshes: u32,
    refuse_refresh: bool,
    refuse_cancel: bool,
}

type Stub = Arc<Mutex<Fake>>;

const CARD: &str = "4000009990001984";

fn answer(fake: &mut Fake, method: &str, path: &str, auth: &str) -> (u16, Value) {
    let request = |fake: &Fake| {
        let mut v = json!({
            "id": "lsrq_abc123", "status": fake.status, "amount": 4250, "currency": "cad",
            "approval_url": "https://app.link.com/activity/approve/lsrq_abc123",
        });
        for (k, extra) in fake.extra.as_object().into_iter().flatten() {
            v[k] = extra.clone();
        }
        v
    };
    match (method, path) {
        ("POST", "/device/token") => {
            if fake.refuse_refresh {
                return (400, json!({"error": "invalid_grant"}));
            }
            fake.refreshes += 1;
            let n = fake.refreshes + 1;
            let fresh = json!({
                "access_token": format!("liwltoken_a{n}"), "refresh_token": format!("liwlrefresh_r{n}"),
                "token_type": "Bearer", "expires_in": 3600,
            });
            (200, fresh)
        }
        // Every API call needs the access token as it is now.
        _ if !auth.starts_with("Bearer liwltoken_a") => (401, json!({"error": "unauthorized"})),
        ("POST", "/spend_requests") => {
            fake.status = "pending_approval".into();
            (200, request(fake))
        }
        ("POST", "/spend_requests/lsrq_abc123/cancel") if fake.refuse_cancel => (
            400,
            json!({"error": {"code": "spend_request_not_cancelable"}}),
        ),
        ("POST", "/spend_requests/lsrq_abc123/cancel") => {
            fake.status = "canceled".into();
            (200, request(fake))
        }
        ("GET", "/spend_requests/lsrq_abc123") => (200, request(fake)),
        ("GET", "/spend_requests/lsrq_abc123?include=card") => {
            let mut v = request(fake);
            if fake.status == "approved" && fake.extra.get("card").is_none() {
                v["card"] = json!({
                    "id": "c1", "brand": "visa", "number": CARD, "cvc": "100", "exp_month": 6,
                    "exp_year": 2029, "billing_address": {"name": "Jenny Rosen", "country": "CA"},
                });
            }
            (200, v)
        }
        _ => (404, json!({"error": "not_found"})),
    }
}

async fn stub() -> (String, Stub) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fake = Stub::default();
    let state = fake.clone();
    tokio::spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let state = state.clone();
            tokio::spawn(async move {
                let svc = hyper::service::service_fn(move |req: Request<Incoming>| {
                    let state = state.clone();
                    async move {
                        let method = req.method().to_string();
                        let path = req
                            .uri()
                            .path_and_query()
                            .map_or("", |p| p.as_str())
                            .to_string();
                        let header = |k: &str| {
                            let v = req.headers().get(k).and_then(|v| v.to_str().ok());
                            v.unwrap_or_default().to_string()
                        };
                        let (auth, kind) = (header("authorization"), header("content-type"));
                        let raw = req.into_body().collect().await.unwrap().to_bytes();
                        let body = match kind.starts_with("application/json") {
                            true => serde_json::from_slice(&raw).unwrap_or(Value::Null),
                            false => json!(String::from_utf8_lossy(&raw)),
                        };
                        let (status, answer) = {
                            let mut fake = state.lock().unwrap();
                            fake.calls
                                .push((method.clone(), path.clone(), auth.clone(), body));
                            answer(&mut fake, &method, &path, &auth)
                        };
                        Ok::<_, std::convert::Infallible>(
                            Response::builder()
                                .status(status)
                                .header("content-type", "application/json")
                                .body(Full::new(Bytes::from(answer.to_string())))
                                .unwrap(),
                        )
                    }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), svc)
                    .await;
            });
        }
    });
    (format!("http://{addr}"), fake)
}

fn home(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "zephyr-link-{name}-{}-{}",
        std::process::id(),
        zephyr::unix_millis()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

const SERVICE: &str = "[[service]]\nname = \"cards\"\nprovider = \"link\"\nkey = \"\"\n\
                       account = \"lwlpk_testclient\"\n";

/// A config whose Link is the stub at `url`, signed in with a token good for `expires_in` s.
fn config(home: &Path, url: &str, expires_in: i64) -> Config {
    let mut cfg: Config = toml::from_str(&format!(
        "rpi = '/r'\nhome = {home:?}\n[cards]\nprovider = 'link'\n"
    ))
    .unwrap();
    cfg.link_api = url.into();
    cfg.link_auth = url.into();
    std::fs::write(home.join("vault.toml"), SERVICE).unwrap();
    let tokens = json!({
        "access_token": "liwltoken_a1", "refresh_token": "liwlrefresh_r1",
        "expires_at": zephyr::now() + expires_in,
    });
    std::fs::write(home.join("cards-link.json"), tokens.to_string()).unwrap();
    cfg
}

fn spec() -> CardSpec {
    CardSpec {
        card: 7,
        cap_minor: 4250,
        currency: "CAD".into(),
        origin: "https://www.amazon.ca".into(),
        merchant: "Amazon.ca".into(),
        expires_at: 1_800_000_000,
    }
}

fn card() -> Card {
    Card {
        id: 7,
        provider_ref: Some("lsrq_abc123".into()),
        ..Card::default()
    }
}

#[tokio::test]
async fn a_spend_request_carries_zephyrs_context_and_answers_the_approval_link() {
    let (url, fake) = stub().await;
    let home = home("issue");
    let link = Link::load(&config(&home, &url, 3600)).unwrap();
    assert!(link.can().asks_owner && link.can().single_use);
    let issued = link.issue(&spec()).await.unwrap();
    assert_eq!(issued.provider_ref, "lsrq_abc123");
    assert_eq!(
        issued.approval_url.as_deref(),
        Some("https://app.link.com/activity/approve/lsrq_abc123")
    );
    let calls = fake.lock().unwrap().calls.clone();
    assert_eq!(calls.len(), 1, "{calls:?}");
    let (method, path, auth, body) = &calls[0];
    assert_eq!(
        (method.as_str(), path.as_str()),
        ("POST", "/spend_requests")
    );
    assert_eq!(auth, "Bearer liwltoken_a1");
    assert_eq!(body["amount"], 4250);
    assert_eq!(body["currency"], "cad");
    assert_eq!(body["merchant_url"], "https://www.amazon.ca");
    assert_eq!(body["merchant_name"], "Amazon.ca");
    assert_eq!(body["credential_type"], "card");
    assert_eq!(body["request_approval"], true);
    assert_eq!(body["idempotency_key"], "zephyr-card-7-1800000000");
    let context = body["context"].as_str().unwrap();
    assert!(context.len() >= 100, "{context}");
    assert!(
        context.starts_with(
            "zephyr card #7: up to 42.50 CAD at https://www.amazon.ca, for \"Amazon.ca\""
        ),
        "{context}"
    );
    // Only what zephyr wrote: no field for the agent's reason, and no test mode unasked.
    assert!(
        body.get("reason").is_none() && body.get("test").is_none(),
        "{body}"
    );
}

#[tokio::test]
async fn the_card_is_revealed_only_once_approved_and_closed_by_a_cancel() {
    let (url, fake) = stub().await;
    let home = home("reveal");
    let link = Link::load(&config(&home, &url, 3600)).unwrap();
    link.issue(&spec()).await.unwrap();
    assert_eq!(link.state(&card()).await, Ok(CardState::Waiting));
    let early = link.reveal(&card()).await.unwrap_err();
    assert_eq!(early.0, "you have not approved it in Link yet");

    fake.lock().unwrap().status = "approved".into();
    assert_eq!(link.state(&card()).await, Ok(CardState::Open));
    let pan = link.reveal(&card()).await.unwrap();
    assert_eq!(format!("{pan:?}"), "Pan(…1984)");
    assert_eq!(
        (pan.number.as_str(), pan.cvc.as_str(), pan.expiry().as_str()),
        (CARD, "100", "06/29")
    );
    assert_eq!(pan.name.as_deref(), Some("Jenny Rosen"));
    // A card that comes without its code is no card to fill.
    fake.lock().unwrap().extra =
        json!({"card": {"number": CARD, "exp_month": 6, "exp_year": 2029}});
    assert_eq!(
        link.reveal(&card()).await.unwrap_err().0,
        "Link gave no complete card"
    );

    link.close(&card()).await.unwrap();
    let calls = fake.lock().unwrap().calls.clone();
    let last = calls.last().unwrap();
    assert_eq!(
        (last.0.as_str(), last.1.as_str()),
        ("POST", "/spend_requests/lsrq_abc123/cancel")
    );
    assert_eq!(link.state(&card()).await, Ok(CardState::Closed));
    // No answer of Link's is ever kept on disk but the tokens.
    let kept = std::fs::read_to_string(home.join("cards-link.json")).unwrap();
    assert!(!kept.contains(CARD), "{kept}");
}

#[tokio::test]
async fn links_statuses_read_as_the_cards_state() {
    let (url, fake) = stub().await;
    let home = home("states");
    let link = Link::load(&config(&home, &url, 3600)).unwrap();
    let set = |status: &str, extra: Value| {
        let mut fake = fake.lock().unwrap();
        (fake.status, fake.extra) = (status.into(), extra);
    };
    for waiting in ["created", "pending_approval", "requires_action"] {
        set(waiting, json!({}));
        assert_eq!(
            link.state(&card()).await,
            Ok(CardState::Waiting),
            "{waiting}"
        );
    }
    set("denied", json!({}));
    assert_eq!(
        link.state(&card()).await,
        Ok(CardState::Failed("you denied it in Link".into()))
    );
    set("expired", json!({}));
    assert_eq!(
        link.state(&card()).await,
        Ok(CardState::Failed("it expired at Link".into()))
    );
    set(
        "failed",
        json!({"payment_status_details": {"outcome": "failure", "decline_code": "insufficient_funds"}}),
    );
    assert_eq!(
        link.state(&card()).await,
        Ok(CardState::Failed(
            "the payment failed at Link: insufficient_funds".into()
        ))
    );
    // A charge is what `payment_status_details` says, in minor units; Link names no acceptor.
    let paid = json!({"payment_status_details": {"outcome": "success", "amount": 4199, "currency": "cad"}});
    for done in ["submitted", "succeeded"] {
        set(done, paid.clone());
        let Ok(CardState::Used(charges)) = link.state(&card()).await else {
            panic!("{done}")
        };
        assert_eq!(
            (charges[0].minor, charges[0].currency.as_str()),
            (4199, "CAD")
        );
        assert_eq!(charges[0].merchant_id, None);
    }
    // A request that is past cancelling because it was paid is closed as it is.
    fake.lock().unwrap().refuse_cancel = true;
    assert_eq!(link.close(&card()).await, Ok(()));
    set("approved", json!({}));
    let refused = link.close(&card()).await.unwrap_err();
    assert_eq!(
        refused.0,
        "Link refused to cancel the request (400): spend_request_not_cancelable"
    );
    set("something_new", json!({}));
    assert!(
        link.state(&card()).await.is_err(),
        "an unknown status is no state"
    );
}

/// Review Focus 6, and the rotation: the new refresh token is on disk, 0600, before the new
/// access token is used.
#[tokio::test]
async fn a_rotated_refresh_token_is_saved_before_the_next_call() {
    let (url, fake) = stub().await;
    let home = home("refresh");
    // Near its end: refreshed first.
    let link = Link::load(&config(&home, &url, 30)).unwrap();
    link.issue(&spec()).await.unwrap();
    let calls = fake.lock().unwrap().calls.clone();
    let seen: Vec<(&str, &str)> = calls.iter().map(|c| (c.1.as_str(), c.2.as_str())).collect();
    assert_eq!(
        seen,
        [
            ("/device/token", ""),
            ("/spend_requests", "Bearer liwltoken_a2")
        ]
    );
    let form = calls[0].3.as_str().unwrap();
    assert_eq!(
        form,
        "grant_type=refresh_token&refresh_token=liwlrefresh_r1&client_id=lwlpk_testclient"
    );
    let file = home.join("cards-link.json");
    let kept: Value = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
    assert_eq!(kept["refresh_token"], "liwlrefresh_r2");
    assert_eq!(kept["access_token"], "liwltoken_a2");
    assert_eq!(
        std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
        0o600
    );
    // Still good: the next call refreshes nothing.
    assert_eq!(link.state(&card()).await, Ok(CardState::Waiting));
    assert_eq!(fake.lock().unwrap().refreshes, 1);
    // A refresh Link refuses asks the owner to connect again, with no token in the words.
    let old =
        json!({"access_token": "liwltoken_a2", "refresh_token": "liwlrefresh_r2", "expires_at": 0});
    std::fs::write(&file, old.to_string()).unwrap();
    fake.lock().unwrap().refuse_refresh = true;
    let e = link.state(&card()).await.unwrap_err().0;
    assert_eq!(e, "connect Link again (zephyr card link)");
}

#[tokio::test]
async fn link_is_not_set_up_without_its_client_id_or_its_tokens() {
    let (url, _fake) = stub().await;
    let home = home("unset");
    let cfg = config(&home, &url, 3600);
    std::fs::remove_file(home.join("cards-link.json")).unwrap();
    let e = Link::load(&cfg).err().unwrap();
    assert_eq!(e, "not run: connect Link again (zephyr card link)");
    std::fs::write(home.join("vault.toml"), "").unwrap();
    let e = Link::load(&cfg).err().unwrap();
    assert_eq!(e, "not run: the card provider is not set up");
    // A service for another provider is not Link's.
    std::fs::write(home.join("vault.toml"), SERVICE.replace("link", "other")).unwrap();
    assert!(Link::load(&cfg).is_err());
}

/// The owner's spike, with their own Link account: `zephyr card link` first, then
/// `ZEPHYR_LINK_LIVE=1 ZEPHYR_LINK_TEST=1 ZEPHYR_CONFIG=<the daemon's zephyr.toml> cargo test -p
/// zephyr --test cards_link -- --ignored --nocapture`. It asks Link for a 1.00 card in test mode
/// (no charge), prints the approval link, waits up to 10 minutes for the approval, fetches the
/// card and cancels the request. What it prints answers the spike's points: whether the
/// request was taken in the account's currency, whether the card came with its code and name,
/// and whether a cancelled request reads `canceled`.
#[tokio::test]
#[ignore]
async fn a_real_link_card_for_one_dollar() {
    if std::env::var("ZEPHYR_LINK_LIVE").as_deref() != Ok("1") {
        eprintln!("skipped: set ZEPHYR_LINK_LIVE=1 and ZEPHYR_CONFIG, after `zephyr card link`");
        return;
    }
    let cfg = Config::load(&Config::default_path()).expect("the daemon's config");
    let link = Link::load(&cfg).expect("zephyr card link first");
    let currency = zephyr::approvals::currency(&cfg);
    let spec = CardSpec {
        card: 0,
        cap_minor: 100,
        currency: currency.clone(),
        origin: "https://press.stripe.com".into(),
        merchant: "Stripe Press (zephyr's live test)".into(),
        expires_at: zephyr::now(),
    };
    let issued = link.issue(&spec).await.expect("a spend request");
    println!(
        "request {} in {currency}; approve it: {:?}",
        issued.provider_ref, issued.approval_url
    );
    let card = Card {
        provider_ref: Some(issued.provider_ref),
        ..Card::default()
    };
    let until = std::time::Instant::now() + std::time::Duration::from_secs(600);
    loop {
        match link.state(&card).await.expect("its state") {
            CardState::Waiting if std::time::Instant::now() < until => {
                tokio::time::sleep(std::time::Duration::from_secs(5)).await
            }
            CardState::Open => break,
            other => panic!("not approved: {other:?}"),
        }
    }
    let pan = link.reveal(&card).await.expect("the card");
    println!(
        "card {pan:?}: {} digits, expiry {}, a code of {} digits, name given: {}",
        pan.number.len(),
        pan.expiry(),
        pan.cvc.len(),
        pan.name.is_some()
    );
    link.close(&card).await.expect("the cancel");
    println!("after the cancel: {:?}", link.state(&card).await);
}
