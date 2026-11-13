//! A card field inside a payment processor's frame (DESIGN §4.25). Stripe Elements and
//! Braintree's Hosted Fields put the card number in a frame of the processor's own origin,
//! which agent-browser 0.41 cannot run a script in (`eval` is always the top document's, and
//! `frame` reaches a cross-origin frame only to `fill` it, with no check in the same step). So
//! the daemon opens its own DevTools session on the agent's Chromium (`get cdp-url`), finds the
//! frames of the tab agent-browser is on, and runs the check-and-type script in the frame's own
//! target, in an isolated world.
//!
//! The script refuses unless, in the one evaluation that types: the frame's own origin is on
//! [`listed`]'s list, its parent is the top, and the top's origin is the card's bound origin
//! (`location.ancestorOrigins`, which Chromium does not mask under a referrer policy, unlike
//! `document.referrer`). The DevTools socket is the agent's, as agent-browser's daemon is:
//! under the threat model that bounds nothing a card's own limits do not, and what it sends
//! back is bounded (`MAX_MESSAGE`, `MAX_MESSAGES`, `WAIT`).

use std::net::SocketAddr;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::WebSocketStream;

use crate::config::Config;
use crate::vault::Typed;

/// The processors' card-field frame origins, from each one's own Content Security Policy
/// guidance (`frame-src`, the origins a checkout must let its card frames load from). Fixed:
/// a change is a code change, with review, never a setting.
/// - Stripe Elements (Stripe.js): `frame-src https://*.js.stripe.com https://js.stripe.com`,
///   https://docs.stripe.com/security/guide#content-security-policy ("Adding *.js.stripe.com
///   allows Stripe.js to improve performance by starting frames on different origins").
/// - Braintree Hosted Fields: `frame-src assets.braintreegateway.com`, sandbox and production,
///   https://braintree.github.io/braintree-web/current/ ("Content Security Policy", "Basic
///   Directives").
///
/// Adyen's guidance asks for `frame-src *` and names no card-field origin, and Shopify's names
/// none either, so neither is here.
const EXACT: &[&str] = &[
    "https://js.stripe.com",
    "https://assets.braintreegateway.com",
];
/// `https://<name>.js.stripe.com`: Stripe's own frames on other origins (the guide above).
const STRIPE_SUFFIX: &str = ".js.stripe.com";

/// Whether `origin` (a bare origin, as `location.origin` prints it) is a processor's
/// card-field frame: exactly one of [`EXACT`], or https on a name under `js.stripe.com`, port
/// 443.
pub fn listed(origin: &str) -> bool {
    let Ok(url) = url::Url::parse(origin) else {
        return false;
    };
    if url.origin().ascii_serialization() != origin || url.scheme() != "https" {
        return false;
    }
    let host = url.host_str().unwrap_or_default();
    EXACT.contains(&origin)
        || (url.port().is_none()
            && host.len() > STRIPE_SUFFIX.len()
            && host.ends_with(STRIPE_SUFFIX))
}

/// Text of the page's (an origin it reports) as one line of at most 200 characters.
fn cut(text: &str) -> String {
    crate::settings::one_line(text, 200)
}

/// What a frame said about itself, in its own document.
#[derive(Debug, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Facts {
    /// `location.origin`: the frame's live address, not its `src`.
    pub origin: String,
    /// `self.origin`: `null` in a sandboxed frame, whatever its address.
    pub self_origin: String,
    /// `window.parent === window.top`, in a frame.
    pub parent_is_top: bool,
    /// `location.ancestorOrigins`, nearest first; `None` where the browser has none.
    pub ancestors: Option<Vec<String>>,
    /// The selector's answer there: `ok`, `missing` or `notfield`.
    pub field: String,
}

/// Why a frame whose document holds the field is not typed into, or `None` when it may be.
pub fn judge(f: &Facts, site: &str) -> Option<String> {
    if f.origin != f.self_origin || !listed(&f.origin) {
        return Some(format!(
            "the frame holding it is on {}, which is not a payment processor's card frame \
             zephyr types into",
            cut(&f.origin)
        ));
    }
    let ancestors = f.ancestors.as_deref().unwrap_or_default();
    if !f.parent_is_top || ancestors.len() != 1 {
        return Some(
            "the frame holding it is inside another frame; only a payment processor's frame \
             directly in the checkout page is filled"
                .into(),
        );
    }
    (ancestors[0] != site).then(|| {
        format!(
            "the payment frame holding it is in a page on {}, not {site}",
            cut(&ancestors[0])
        )
    })
}

/// The checks every in-frame script starts with, against `frame` (the frame's origin, the
/// listed one it was found on) and `want` (the card's origin).
const CHECKS: &str = r#"if (window === window.top || window.parent !== window.top) return "nested";
if (location.origin !== frame || self.origin !== frame) return "left:" + location.origin;
const above = location.ancestorOrigins;
if (!above || above.length !== 1) return "nested";
if (above[0] !== want) return "moved:" + above[0];"#;

/// What a frame says about itself and the selector, typing nothing (see [`Facts`]).
fn probe_script(selector: &str) -> String {
    format!(
        r#"(() => {{
const sel = {};
let field = "missing";
try {{
  const el = document.querySelector(sel);
  field = !el ? "missing" : (el instanceof HTMLInputElement || el instanceof HTMLTextAreaElement || el instanceof HTMLSelectElement) ? "ok" : "notfield";
}} catch (e) {{}}
return JSON.stringify({{
  origin: location.origin, selfOrigin: self.origin,
  parentIsTop: window !== window.top && window.parent === window.top,
  ancestors: location.ancestorOrigins ? Array.from(location.ancestorOrigins) : null,
  field,
}});
}})()"#,
        json!(selector)
    )
}

/// A buffer that holds a card value: allocated once at its full size, so it never moves (and
/// leaves no copy behind), and wiped when dropped, so a fill whose future is dropped wipes it
/// too.
pub(crate) struct Secret(String);

impl Secret {
    /// Takes `value` as it is (no copy is made), to be wiped when dropped.
    pub(crate) fn new(value: String) -> Secret {
        Secret(value)
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }

    fn with_capacity(n: usize) -> Secret {
        Secret(String::with_capacity(n))
    }

    /// Appends `s`; never past the capacity reserved, so the buffer is never reallocated.
    fn push(&mut self, s: &str) -> Result<(), String> {
        if self.0.len() + s.len() > self.0.capacity() {
            return Err("the typing script did not fit its buffer".into());
        }
        self.0.push_str(s);
        Ok(())
    }

    /// Appends `s` as the inside of a JSON string, which is a JavaScript string too: `"` and
    /// `\` escaped, control characters and the two JavaScript line separators as `\uXXXX`.
    fn push_escaped(&mut self, s: &str) -> Result<(), String> {
        let mut unit = [0u8; 4];
        for c in s.chars() {
            let hex;
            let piece = match c {
                '"' => "\\\"",
                '\\' => "\\\\",
                c if (c as u32) < 0x20 || c == '\u{2028}' || c == '\u{2029}' => {
                    hex = format!("\\u{:04x}", c as u32);
                    &hex
                }
                c => c.encode_utf8(&mut unit),
            };
            self.push(piece)?;
        }
        Ok(())
    }
}

impl Drop for Secret {
    fn drop(&mut self) {
        crate::vault::wipe_str(&mut self.0);
    }
}

/// The most [`Secret::push_escaped`] makes of `n` bytes: `\u001f` for each.
fn escaped(n: usize) -> usize {
    6 * n
}

/// The script that types one value into the frame's field: [`CHECKS`] first, in the same
/// evaluation as the typing, so nothing can move between a check and the value it guards.
fn type_script(want: &str, frame: &str, selector: &str, value: &str) -> Result<Secret, String> {
    let head = format!(
        "(() => {{\nconst want = {};\nconst frame = {};\nconst sel = {};\nconst value = \"",
        json!(want),
        json!(frame),
        json!(selector),
    );
    let tail = format!(
        "\";\n{CHECKS}\n{}\nreturn \"ok\";\n}})()",
        crate::vault::SET_FIELD
    );
    let mut script = Secret::with_capacity(head.len() + escaped(value.len()) + tail.len());
    script.push(&head)?;
    script.push_escaped(value)?;
    script.push(&tail)?;
    Ok(script)
}

/// How long each frame step may take: what one agent-browser command may.
const WAIT: Duration = Duration::from_secs(20);
/// The largest DevTools message or frame read: these replies are a few kilobytes.
const MAX_MESSAGE: usize = 256 << 10;
/// The most messages read while waiting for one reply (events in between are skipped).
const MAX_MESSAGES: usize = 200;

/// `url` (`get cdp-url`'s answer) as the address to connect to: only a `ws://` address with a
/// port on this machine's loopback.
fn devtools_addr(url: &str) -> Option<SocketAddr> {
    let parsed = url::Url::parse(url).ok()?;
    let port = parsed.port()?;
    if parsed.scheme() != "ws" {
        return None;
    }
    match parsed.host()? {
        url::Host::Ipv4(ip) if ip.is_loopback() => Some((ip, port).into()),
        url::Host::Ipv6(ip) if ip.is_loopback() => Some((ip, port).into()),
        url::Host::Domain("localhost") => Some(([127, 0, 0, 1], port).into()),
        _ => None,
    }
}

/// A message's `id`, read without building the rest of it.
#[derive(serde::Deserialize)]
struct Envelope {
    id: Option<u64>,
}

/// One DevTools session over the browser's own socket.
struct Cdp {
    ws: WebSocketStream<TcpStream>,
    next: u64,
}

impl Cdp {
    async fn connect(url: &str) -> Result<Cdp, String> {
        let addr =
            devtools_addr(url).ok_or("the browser's DevTools address is not on this machine")?;
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connecting to the browser's DevTools: {e}"))?;
        let config = WebSocketConfig {
            max_message_size: Some(MAX_MESSAGE),
            max_frame_size: Some(MAX_MESSAGE),
            ..Default::default()
        };
        let (ws, _) = tokio_tungstenite::client_async_with_config(url, tcp, Some(config))
            .await
            .map_err(|e| format!("opening the browser's DevTools: {e}"))?;
        Ok(Cdp { ws, next: 0 })
    }

    async fn send(&mut self, text: String) -> Result<(), String> {
        let sent = self.ws.send(Message::Text(text)).await;
        sent.map_err(|e| format!("DevTools: {e}"))
    }

    /// The `result` of the reply to `id`, skipping at most [`MAX_MESSAGES`] others.
    async fn reply(&mut self, id: u64, method: &str) -> Result<Value, String> {
        for _ in 0..MAX_MESSAGES {
            let Some(frame) = self.ws.next().await else {
                return Err("the browser's DevTools closed".into());
            };
            let Message::Text(text) = frame.map_err(|e| format!("DevTools: {e}"))? else {
                continue;
            };
            let Ok(Envelope { id: Some(got) }) = serde_json::from_str(&text) else {
                continue;
            };
            if got != id {
                continue;
            }
            let mut reply: Value =
                serde_json::from_str(&text).map_err(|_| format!("DevTools garbled {method}"))?;
            if !reply["error"].is_null() {
                return Err(format!("DevTools refused {method}"));
            }
            return Ok(reply["result"].take());
        }
        Err(format!("DevTools sent too much before answering {method}"))
    }

    /// `method` with `params` (on `session`, a target attached with `flatten`): its `result`.
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next += 1;
        let mut msg = json!({"id": self.next, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        self.send(msg.to_string()).await?;
        self.reply(self.next, method).await
    }

    /// What `out` (a `Runtime.evaluate` result) evaluated to, as a string.
    fn value(out: &Value) -> Result<String, String> {
        if !out["exceptionDetails"].is_null() {
            return Err("the payment frame refused the script".into());
        }
        Ok(out["result"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }

    /// `expression` (which holds no secret) in `world` of `session`.
    async fn eval(
        &mut self,
        session: &str,
        world: i64,
        expression: &str,
    ) -> Result<String, String> {
        let params = json!({"expression": expression, "contextId": world, "returnByValue": true});
        Self::value(&self.call("Runtime.evaluate", params, Some(session)).await?)
    }

    /// `script` (which holds a card value) in `world` of `session`: the message is built in a
    /// [`Secret`] of its own, never a `Value`.
    async fn eval_secret(
        &mut self,
        session: &str,
        world: i64,
        script: &Secret,
    ) -> Result<String, String> {
        self.next += 1;
        let head = format!(
            r#"{{"id":{},"method":"Runtime.evaluate","sessionId":{},"params":{{"contextId":{world},"returnByValue":true,"expression":""#,
            self.next,
            json!(session)
        );
        let tail = "\"}}";
        let mut msg = Secret::with_capacity(head.len() + escaped(script.0.len()) + tail.len());
        msg.push(&head)?;
        msg.push_escaped(&script.0)?;
        msg.push(tail)?;
        // shortcut: tungstenite takes the text by value and frees its copy (and the frame it
        // builds) unwiped; wiping those needs a writer of our own.
        self.send(msg.0.clone()).await?;
        drop(msg);
        Self::value(&self.reply(self.next, "Runtime.evaluate").await?)
    }
}

/// A frame of the active tab, attached, with its isolated world.
struct Frame {
    session: String,
    world: i64,
}

/// The DevTools session of one card fill: the listed frames directly in the tab agent-browser
/// is on, found once ([`Frames::open`]) and asked about each field ([`Frames::find`]).
pub struct Frames {
    cdp: Cdp,
    site: String,
    frames: Vec<Frame>,
}

/// `fut`, or `Err` once [`WAIT`] has passed.
async fn bounded<T>(
    fut: impl std::future::Future<Output = Result<T, String>>,
) -> Result<T, String> {
    tokio::time::timeout(WAIT, fut)
        .await
        .unwrap_or_else(|_| Err("the browser's DevTools did not answer in time".into()))
}

/// The target id of the tab agent-browser is on (its `tab` list), which must be on `site`: no
/// active tab, or more than one, refuses.
fn active_tab(listing: &str, site: &str) -> Result<String, String> {
    let listing: Value = serde_json::from_str(listing.trim()).unwrap_or_default();
    let tabs = listing["data"]["tabs"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    let active: Vec<&Value> = tabs.iter().filter(|t| t["active"] == true).collect();
    let [tab] = active.as_slice() else {
        return Err("agent-browser does not say which one tab it is on".into());
    };
    let url = tab["url"].as_str().unwrap_or_default();
    if crate::vault::origin(url).is_none_or(|o| o.ascii_serialization() != site) {
        return Err(format!("the tab agent-browser is on is not on {site}"));
    }
    match tab["targetId"].as_str() {
        Some(id) if !id.is_empty() => Ok(id.to_string()),
        _ => Err("agent-browser does not say which one tab it is on".into()),
    }
}

impl Frames {
    /// Connects to the agent's Chromium and finds the frames of the tab agent-browser is on
    /// ([`Frames::discover`]).
    pub async fn open(cfg: &Config, site: &str) -> Result<Frames, String> {
        bounded(async {
            let url = crate::vault::browser(cfg, &["get", "cdp-url"])
                .await
                .map_err(|e| format!("the page's frames could not be looked into: {e}"))?;
            let mut frames = Frames {
                cdp: Cdp::connect(url.trim()).await?,
                site: site.to_string(),
                frames: Vec::new(),
            };
            frames.discover(cfg).await?;
            Ok(frames)
        })
        .await
    }

    /// Attaches, on this session, to each frame on a listed origin whose parent is the tab
    /// agent-browser is on, a page on the card's site, in place of the frames found before
    /// (those are let go). A frame that cannot be attached is left out. Only a prefilter: a
    /// target's address may be one it is leaving, so each frame's own answer is what
    /// [`Frames::find`] decides on.
    async fn discover(&mut self, cfg: &Config) -> Result<(), String> {
        let site = self.site.clone();
        for old in std::mem::take(&mut self.frames) {
            let detach = json!({"sessionId": old.session});
            let _ = self.cdp.call("Target.detachFromTarget", detach, None).await;
        }
        let tab = crate::vault::browser(cfg, &["tab", "list", "--json"])
            .await
            .map_err(|e| format!("the page's frames could not be looked into: {e}"))?;
        let tab = active_tab(&tab, &site)?;
        let targets = self.cdp.call("Target.getTargets", json!({}), None).await?;
        let all = targets["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let on = |t: &Value| crate::vault::origin(t["url"].as_str().unwrap_or_default());
        let page = all.iter().any(|t| {
            t["type"] == "page"
                && t["targetId"] == tab.as_str()
                && on(t).is_some_and(|o| o.ascii_serialization() == site)
        });
        if !page {
            return Err(format!("the tab agent-browser is on is not on {site}"));
        }
        let ids: Vec<String> = (all.iter())
            .filter(|t| t["type"] == "iframe" && t["parentFrameId"] == tab.as_str())
            .filter(|t| on(t).is_some_and(|o| listed(&o.ascii_serialization())))
            .filter_map(|t| t["targetId"].as_str().map(String::from))
            .collect();
        for id in ids {
            let attach = json!({"targetId": id, "flatten": true});
            let Ok(attached) = self.cdp.call("Target.attachToTarget", attach, None).await else {
                continue;
            };
            let session = attached["sessionId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            // An isolated world: the frame's own scripts cannot have changed what this one
            // reads (its prototypes, `Array.from`).
            let world = json!({"frameId": id, "worldName": "zephyr-card"});
            let made = self
                .cdp
                .call("Page.createIsolatedWorld", world, Some(&session));
            let Ok(world) = made.await else {
                continue;
            };
            if let Some(world) = world["executionContextId"].as_i64() {
                self.frames.push(Frame { session, world });
            }
        }
        Ok(())
    }

    /// Asks every frame about `selector`: the frames whose document holds it, and whether any
    /// could not be asked.
    async fn look(&mut self, probe: &str) -> (Vec<(usize, Facts)>, bool) {
        let (mut holding, mut failed) = (Vec::new(), false);
        for (i, frame) in self.frames.iter().enumerate() {
            let said = self.cdp.eval(&frame.session, frame.world, probe).await;
            let Some(facts) = said
                .ok()
                .and_then(|s| serde_json::from_str::<Facts>(&s).ok())
            else {
                failed = true;
                continue;
            };
            if facts.field != "missing" {
                holding.push((i, facts));
            }
        }
        (holding, failed)
    }

    /// The one frame whose own document holds `selector`, and its origin: `Ok(None)` when no
    /// frame does. When none holds it, or a frame could not be asked, the tab's frames are
    /// found again (a processor may mount a field's frame late, or remount one) and asked once
    /// more. A frame that still cannot be asked is passed over; two that hold it, or one that
    /// fails [`judge`] (another origin, a nested one, a top elsewhere), refuse the field.
    pub async fn find(
        &mut self,
        cfg: &Config,
        selector: &str,
    ) -> Result<Option<(usize, String)>, String> {
        let probe = probe_script(selector);
        bounded(async {
            let (mut holding, failed) = self.look(&probe).await;
            if holding.is_empty() || failed {
                self.discover(cfg).await?;
                holding = self.look(&probe).await.0;
            }
            if holding.len() > 1 {
                return Err("that selector matches a field in more than one payment frame".into());
            }
            let Some((i, facts)) = holding.pop() else {
                return Ok(None);
            };
            match judge(&facts, &self.site) {
                Some(why) => Err(why),
                None => Ok(Some((i, facts.origin))),
            }
        })
        .await
    }

    /// Types `value` into `selector` of frame `at`, if that frame is still on `origin`,
    /// directly in a top on the card's site (`Typed`).
    pub async fn type_value(
        &mut self,
        at: usize,
        origin: &str,
        selector: &str,
        value: &str,
    ) -> Typed {
        let Some(Frame { session, world }) = self.frames.get(at) else {
            return Typed::Failed("the payment frame is gone".into());
        };
        let (session, world) = (session.clone(), *world);
        let said = bounded(async {
            let script = type_script(&self.site, origin, selector, value)?;
            self.cdp.eval_secret(&session, world, &script).await
        });
        let said = match said.await {
            Ok(said) => said,
            Err(e) => return Typed::Failed(e),
        };
        match said.as_str() {
            "ok" => Typed::Ok,
            "missing" => Typed::Missing,
            "notfield" => Typed::NotField,
            "nested" => Typed::Failed("the payment frame is no longer directly in the page".into()),
            r => match (r.strip_prefix("moved:"), r.strip_prefix("left:")) {
                (Some(on), _) => Typed::Moved(cut(on)),
                (_, Some(on)) => Typed::Failed(format!(
                    "the payment frame had moved to {}, not {origin}; nothing was typed",
                    cut(on)
                )),
                _ => Typed::Failed("the payment frame gave no answer".into()),
            },
        }
    }

    /// Ends the session, within [`WAIT`]; the socket is dropped either way.
    pub async fn close(mut self) {
        let _ = tokio::time::timeout(WAIT, self.cdp.ws.close(None)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A DevTools server of the agent's that answers with `flood` (a few huge messages, or a
    /// stream of events that are never the reply): the call fails within a second or two, and
    /// nothing it sent was kept whole (`MAX_MESSAGE`, `MAX_MESSAGES`).
    async fn flooded(flood: impl Fn(u64) -> Vec<String> + Send + 'static) -> Result<Value, String> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut ws = tokio_tungstenite::accept_async(tcp).await.unwrap();
            while let Some(Ok(Message::Text(text))) = ws.next().await {
                let id = serde_json::from_str::<Value>(&text).unwrap()["id"]
                    .as_u64()
                    .unwrap();
                for m in flood(id) {
                    if ws.send(Message::Text(m)).await.is_err() {
                        return;
                    }
                }
            }
        });
        let url = format!("ws://127.0.0.1:{port}/devtools/browser/x");
        let mut cdp = Cdp::connect(&url).await.unwrap();
        let start = std::time::Instant::now();
        let out = cdp.call("Target.getTargets", json!({}), None).await;
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "{:?}",
            start.elapsed()
        );
        out
    }

    #[tokio::test]
    async fn a_devtools_server_that_floods_the_daemon_is_dropped_fast() {
        // One reply of 64 MiB: refused as it comes, never read whole.
        let huge = flooded(|id| {
            vec![format!(
                r#"{{"id":{id},"result":{{"x":"{}"}}}}"#,
                "a".repeat(64 << 20)
            )]
        });
        assert!(huge.await.is_err());
        // Events and other replies, and never the one asked for.
        let events = flooded(|id| {
            let event = format!(
                r#"{{"method":"Ev","params":{{"x":"{}"}}}}"#,
                "a".repeat(1 << 10)
            );
            let other = format!(r#"{{"id":{},"result":{{}}}}"#, id + 1);
            [event, other].into_iter().cycle().take(1000).collect()
        });
        assert!(events.await.is_err());
    }

    #[test]
    fn only_the_processors_card_frame_origins_are_listed() {
        for yes in [
            "https://js.stripe.com",
            "https://b.js.stripe.com",
            "https://m-outer-3437aaddcdf6922d623e172c2d6f9278.js.stripe.com",
            "https://assets.braintreegateway.com",
        ] {
            assert!(listed(yes), "{yes}");
        }
        for no in [
            "http://js.stripe.com",
            "https://js.stripe.com:444",
            "https://js.stripe.com/",
            "https://.js.stripe.com",
            "https://js.stripe.com.evil.example",
            "https://evil-js.stripe.com",
            "https://jsxstripe.com",
            "https://stripe.com",
            "https://checkout.stripe.com",
            "https://b.assets.braintreegateway.com",
            "https://assets.braintreegateway.com.evil.example",
            "https://checkoutshopper-live.adyen.com",
            "https://checkout.pci.shopifyinc.com",
            "null",
            "",
        ] {
            assert!(!listed(no), "{no}");
        }
    }

    fn facts(origin: &str, top: bool, ancestors: &[&str]) -> Facts {
        Facts {
            origin: origin.into(),
            self_origin: origin.into(),
            parent_is_top: top,
            ancestors: Some(ancestors.iter().map(|s| s.to_string()).collect()),
            field: "ok".into(),
        }
    }

    #[test]
    fn a_frame_is_typed_into_only_on_a_listed_origin_directly_under_the_bound_top() {
        let shop = "https://shop.example";
        let stripe = "https://js.stripe.com";
        assert_eq!(judge(&facts(stripe, true, &[shop]), shop), None);
        let why = |f: Facts| judge(&f, shop).unwrap();
        assert!(why(facts("https://evil.example", true, &[shop])).contains("not a payment"));
        let nested = facts(stripe, false, &["https://mid.example", shop]);
        assert!(why(nested).contains("inside another frame"));
        // A parent that says it is the top, but two ancestors: still nested.
        assert!(why(facts(stripe, true, &["https://mid.example", shop])).contains("inside"));
        let under = why(facts(stripe, true, &["https://evil.example"]));
        assert!(under.contains("in a page on https://evil.example, not https://shop"));
        // A sandboxed frame's own origin is opaque, whatever its address.
        let mut sandboxed = facts(stripe, true, &[shop]);
        sandboxed.self_origin = "null".into();
        assert!(why(sandboxed).contains("not a payment"));
        // No ancestor list at all: fails closed.
        let mut none = facts(stripe, true, &[shop]);
        none.ancestors = None;
        assert!(why(none).contains("inside another frame"));
    }

    #[test]
    fn the_frame_script_checks_everything_before_it_types() {
        let shop = "https://shop.example";
        let script = type_script(shop, "https://js.stripe.com", "#n", "4\"2\\4\u{2028}2").unwrap();
        let s = script.0.clone();
        let at = |needle: &str| s.find(needle).unwrap_or_else(|| panic!("{needle}\n{s}"));
        let set = at(".set.call(el, value)");
        for check in [
            "window.parent !== window.top",
            "location.origin !== frame",
            "self.origin",
            "above.length !== 1",
            "above[0] !== want",
        ] {
            assert!(
                at(check) < set,
                "{check} is checked before the value is set"
            );
        }
        // The value is one JSON (and JavaScript) string, whatever it holds.
        let line = s.lines().find(|l| l.starts_with("const value = ")).unwrap();
        let value: String = serde_json::from_str(&line[14..line.len() - 1]).unwrap();
        assert_eq!(value, "4\"2\\4\u{2028}2");
    }
}
