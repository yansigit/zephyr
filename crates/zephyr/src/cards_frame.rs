//! A card field inside a payment processor's frame (DESIGN §4.25). Stripe Elements and
//! Braintree's Hosted Fields put the card number in a frame of the processor's own origin,
//! which agent-browser 0.41 cannot run a script in (`eval` is always the top document's, and
//! `frame` reaches a cross-origin frame only to `fill` it, with no check in the same step). So
//! the daemon opens its own DevTools session on the agent's Chromium (`get cdp-url`), finds the
//! frame's own target, and runs the check-and-type script there, in an isolated world.
//!
//! The script refuses unless, in the one evaluation that types: the frame's own origin is on
//! [`listed`]'s list, its parent is the top, and the top's origin is the card's bound origin
//! (`location.ancestorOrigins`, which Chromium does not mask under a referrer policy, unlike
//! `document.referrer`). The DevTools socket is the agent's, as agent-browser's daemon is:
//! under the threat model that bounds nothing a card's own limits do not.

use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpStream;
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
    let cut = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).take(200).collect() };
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

/// The script that types one value into the frame's field: [`CHECKS`] first, in the same
/// evaluation as the typing, so nothing can move between a check and the value it guards.
fn type_script(want: &str, frame: &str, selector: &str, value: &str) -> String {
    format!(
        "(() => {{\nconst want = {};\nconst frame = {};\nconst sel = {};\nconst value = {};\n\
         {CHECKS}\n{}\nreturn \"ok\";\n}})()",
        json!(want),
        json!(frame),
        json!(selector),
        json!(value),
        crate::vault::SET_FIELD
    )
}

/// How long the whole frame step may take: what one agent-browser command may.
const WAIT: Duration = Duration::from_secs(20);

/// One DevTools session over the browser's own socket.
struct Cdp {
    ws: WebSocketStream<TcpStream>,
    next: u64,
}

impl Cdp {
    /// `url` is `get cdp-url`'s answer: only a `ws://` address on this machine's loopback.
    async fn connect(url: &str) -> Result<Cdp, String> {
        let bad = || "the browser's DevTools address is not on this machine".to_string();
        let parsed = url::Url::parse(url).map_err(|_| bad())?;
        let loopback = match parsed.host() {
            Some(url::Host::Ipv4(ip)) => ip.is_loopback(),
            Some(url::Host::Ipv6(ip)) => ip.is_loopback(),
            Some(url::Host::Domain(d)) => d == "localhost",
            None => false,
        };
        let port = parsed.port().ok_or_else(bad)?;
        if parsed.scheme() != "ws" || !loopback {
            return Err(bad());
        }
        let addr = match parsed.host() {
            Some(url::Host::Ipv6(ip)) => std::net::SocketAddr::from((ip, port)),
            Some(url::Host::Ipv4(ip)) => std::net::SocketAddr::from((ip, port)),
            _ => std::net::SocketAddr::from(([127, 0, 0, 1], port)),
        };
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connecting to the browser's DevTools: {e}"))?;
        let (ws, _) = tokio_tungstenite::client_async(url, tcp)
            .await
            .map_err(|e| format!("opening the browser's DevTools: {e}"))?;
        Ok(Cdp { ws, next: 0 })
    }

    /// `method` with `params` (on `session`, a target attached with `flatten`): its `result`.
    /// Events in between are skipped.
    async fn call(
        &mut self,
        method: &str,
        params: Value,
        session: Option<&str>,
    ) -> Result<Value, String> {
        self.next += 1;
        let id = self.next;
        let mut msg = json!({"id": id, "method": method, "params": params});
        if let Some(s) = session {
            msg["sessionId"] = json!(s);
        }
        // shortcut: the frame tungstenite builds from this text is freed unwiped, like the
        // browser's own copies; wiping it needs a writer of our own.
        let text = msg.to_string();
        drop(msg);
        let sent = self.ws.send(Message::Text(text)).await;
        sent.map_err(|e| format!("DevTools: {e}"))?;
        while let Some(frame) = self.ws.next().await {
            let Message::Text(text) = frame.map_err(|e| format!("DevTools: {e}"))? else {
                continue;
            };
            let Ok(mut reply) = serde_json::from_str::<Value>(&text) else {
                continue;
            };
            if reply["id"].as_u64() != Some(id) {
                continue;
            }
            if !reply["error"].is_null() {
                return Err(format!("DevTools refused {method}"));
            }
            return Ok(reply["result"].take());
        }
        Err("the browser's DevTools closed".into())
    }

    /// `expression`'s value in `world` of `session`, as a string.
    async fn eval(
        &mut self,
        session: &str,
        world: i64,
        expression: &str,
    ) -> Result<String, String> {
        let params = json!({"expression": expression, "contextId": world, "returnByValue": true});
        let out = self.call("Runtime.evaluate", params, Some(session)).await?;
        if !out["exceptionDetails"].is_null() {
            return Err("the payment frame refused the script".into());
        }
        Ok(out["result"]["value"]
            .as_str()
            .unwrap_or_default()
            .to_string())
    }
}

/// The one payment frame whose document holds the field, attached and ready to type into.
pub struct Found {
    cdp: Cdp,
    session: String,
    world: i64,
    /// The frame's own origin, as it said: what the read-back flag lists before typing.
    pub origin: String,
}

/// The payment frame directly in the top page on `site` whose own document holds `selector`:
/// `Ok(None)` when no listed frame does. A frame that holds it but fails [`judge`] (another
/// origin, a nested one, a top elsewhere), or two that hold it, refuse the field.
pub async fn find(cfg: &Config, site: &str, selector: &str) -> Result<Option<Found>, String> {
    let step = async {
        let url = crate::vault::browser(cfg, &["get", "cdp-url"])
            .await
            .map_err(|e| format!("the page's frames could not be looked into: {e}"))?;
        let mut cdp = Cdp::connect(url.trim()).await?;
        let targets = cdp.call("Target.getTargets", json!({}), None).await?;
        let all = targets["targetInfos"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        let on = |t: &Value| crate::vault::origin(t["url"].as_str().unwrap_or_default());
        let tops: Vec<&str> = (all.iter())
            .filter(|t| t["type"] == "page")
            .filter(|t| on(t).is_some_and(|o| o.ascii_serialization() == site))
            .filter_map(|t| t["targetId"].as_str())
            .collect();
        // Only a prefilter: a target's address may be one it is leaving. The frame's own
        // answer below is what decides.
        let frames: Vec<String> = (all.iter())
            .filter(|t| t["type"] == "iframe")
            .filter(|t| tops.contains(&t["parentFrameId"].as_str().unwrap_or_default()))
            .filter(|t| on(t).is_some_and(|o| listed(&o.ascii_serialization())))
            .filter_map(|t| t["targetId"].as_str().map(String::from))
            .collect();
        let mut holding: Vec<(String, i64, Facts)> = Vec::new();
        let probe = probe_script(selector);
        for frame in frames {
            let attach = json!({"targetId": frame, "flatten": true});
            let attached = cdp.call("Target.attachToTarget", attach, None).await?;
            let session = attached["sessionId"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            // An isolated world: the frame's own scripts cannot have changed what this one
            // reads (its prototypes, `Array.from`).
            let world = json!({"frameId": frame, "worldName": "zephyr-card"});
            let world = cdp
                .call("Page.createIsolatedWorld", world, Some(&session))
                .await?;
            let world = world["executionContextId"].as_i64().unwrap_or_default();
            let said = cdp.eval(&session, world, &probe).await?;
            let facts: Facts = serde_json::from_str(&said)
                .map_err(|_| "the payment frame gave no answer".to_string())?;
            if facts.field == "missing" {
                let detach = json!({"sessionId": session});
                let _ = cdp.call("Target.detachFromTarget", detach, None).await;
                continue;
            }
            holding.push((session, world, facts));
        }
        if holding.len() > 1 {
            return Err("that selector matches a field in more than one payment frame".into());
        }
        let Some((session, world, facts)) = holding.pop() else {
            return Ok(None);
        };
        if let Some(why) = judge(&facts, site) {
            return Err(why);
        }
        Ok(Some(Found {
            cdp,
            session,
            world,
            origin: facts.origin,
        }))
    };
    match tokio::time::timeout(WAIT, step).await {
        Ok(found) => found,
        Err(_) => Err("the browser's DevTools did not answer in time".into()),
    }
}

impl Found {
    /// Types `value` into `selector` of this frame, if the frame is still on its origin,
    /// directly in a top on `site` (`Typed`).
    pub async fn type_value(mut self, site: &str, selector: &str, value: &str) -> Typed {
        let mut script = type_script(site, &self.origin, selector, value);
        let said = tokio::time::timeout(WAIT, self.cdp.eval(&self.session, self.world, &script));
        let said = said.await;
        crate::vault::wipe_str(&mut script);
        let _ = self.cdp.ws.close(None).await;
        let said = match said {
            Ok(Ok(said)) => said,
            Ok(Err(e)) => return Typed::Failed(e),
            Err(_) => return Typed::Failed("the browser's DevTools did not answer in time".into()),
        };
        let cut = |s: &str| -> String { s.chars().filter(|c| !c.is_control()).take(200).collect() };
        match said.as_str() {
            "ok" => Typed::Ok,
            "missing" => Typed::Missing,
            "notfield" => Typed::NotField,
            "nested" => Typed::Failed("the payment frame is no longer directly in the page".into()),
            r => match (r.strip_prefix("moved:"), r.strip_prefix("left:")) {
                (Some(on), _) => Typed::Moved(cut(on)),
                (_, Some(on)) => Typed::Failed(format!(
                    "the payment frame had moved to {}, not {}; nothing was typed",
                    cut(on),
                    self.origin
                )),
                _ => Typed::Failed("the payment frame gave no answer".into()),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let s = type_script(
            "https://shop.example",
            "https://js.stripe.com",
            "#n",
            "4242",
        );
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
        assert!(s.contains("const value = \"4242\";"));
    }
}
