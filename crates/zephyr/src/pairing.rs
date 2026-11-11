//! Pairing codes (DESIGN §4.3): how a Telegram or Signal owner is learned. A strong channel asks
//! for a one-time code (`/pair telegram`), and the sender of the message that carries it back
//! becomes an owner of that channel. Only the code's SHA-256 is kept. Pure: the router stores
//! the row, counts wrong codes and edits the owner list.

use crate::web::passkey::sha256;
use crate::web::signin::{random, typed};
use data_encoding::{BASE32_NOPAD, HEXLOWER};
use subtle::ConstantTimeEq;

/// How long a code works, in seconds.
pub const LIFE: i64 = 10 * 60;
/// The channels whose owners are paired.
pub const CHANNELS: [&str; 2] = ["telegram", "signal"];

/// The `meta` key holding `channel`'s code.
pub fn meta_key(channel: &str) -> String {
    format!("pair_{channel}")
}

/// A new code, as shown (`ABCDE-FGHJK`), and the `meta` row to keep: its hash and expiry.
pub fn issue(now: i64) -> (String, String) {
    // 6 random bytes: 48 bits, 10 characters a phone can type.
    let code = BASE32_NOPAD.encode(&random::<6>());
    let row = serde_json::json!({
        "sha256": HEXLOWER.encode(&sha256(code.as_bytes())),
        "expires": now + LIFE,
    });
    (format!("{}-{}", &code[..5], &code[5..]), row.to_string())
}

/// The code a message on `channel` claims with, when it is a pairing channel's.
pub fn claim_on(channel: &str, text: &str) -> Option<String> {
    CHANNELS.contains(&channel).then(|| claimed(text)).flatten()
}

/// The code a message claims with, as typed: its whole text is `/pair CODE`.
pub fn claimed(text: &str) -> Option<String> {
    let code = typed(text.trim().strip_prefix("/pair ")?);
    (!code.is_empty() && code.len() <= 32).then_some(code)
}

/// Whether `code` (as `claimed` gives it) is the unexpired one `row` was issued for.
pub fn matches(row: &str, code: &str, now: i64) -> bool {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(row) else {
        return false;
    };
    let hash = v["sha256"]
        .as_str()
        .and_then(|h| HEXLOWER.decode(h.as_bytes()).ok());
    let live = v["expires"].as_i64().is_some_and(|at| at > now);
    let same = hash.is_some_and(|h| bool::from(h.ct_eq(&sha256(code.as_bytes()))));
    live && same
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_matches_typed_any_way() {
        let (shown, row) = issue(1000);
        assert!(shown.len() == 11 && shown.as_bytes()[5] == b'-', "{shown}");
        for typed in [shown.clone(), shown.to_lowercase(), shown.replace('-', " ")] {
            let code = claimed(&format!("/pair {typed}")).unwrap();
            assert!(matches(&row, &code, 1000), "{typed}");
        }
        assert!(!matches(&row, "AAAAAAAAAA", 1000));
        assert!(!matches(
            "not json",
            &claimed(&format!("/pair {shown}")).unwrap(),
            1000
        ));
    }

    #[test]
    fn a_code_expires() {
        let (shown, row) = issue(1000);
        let code = claimed(&format!("/pair {shown}")).unwrap();
        assert!(matches(&row, &code, 1000 + LIFE - 1));
        assert!(!matches(&row, &code, 1000 + LIFE));
    }

    #[test]
    fn claimed_needs_the_whole_text() {
        assert_eq!(claimed("  /pair abcde-fghjk "), Some("ABCDEFGHJK".into()));
        assert_eq!(claimed("/pair"), None);
        assert_eq!(claimed("/pair  "), None);
        assert_eq!(claimed("please /pair ABCDE-FGHJK"), None);
        assert_eq!(claimed("/paired ABCDE"), None);
        assert_eq!(claimed(&format!("/pair {}", "A".repeat(33))), None);
    }
}
