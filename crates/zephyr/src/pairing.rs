//! One-time codes: pairing a Telegram or Signal owner (DESIGN §4.3) and enrolling a web app
//! passkey (§4.3.3). A strong channel asks for a code (`/pair telegram`, `/web enrol`), and what
//! carries it back is trusted as that channel was. Only the code's SHA-256 is kept, in a `meta`
//! row with its expiry. Pure: callers store the row, count wrong codes and act on a match.

use crate::web::passkey::sha256;
use crate::web::signin::random;
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

/// A new code, as shown (`ABCDE-FGHJK`), and the `meta` row to keep: its hash, its expiry and
/// the name it was made for (a passkey's), if any.
pub fn issue(now: i64, name: Option<&str>) -> (String, String) {
    // 6 random bytes: 48 bits, 10 characters a phone can type.
    let code = BASE32_NOPAD.encode(&random::<6>());
    let row = serde_json::json!({
        "sha256": HEXLOWER.encode(&sha256(code.as_bytes())),
        "name": name,
        "expires": now + LIFE,
    });
    (format!("{}-{}", &code[..5], &code[5..]), row.to_string())
}

/// The code `row` holds, while it still works: its hash and the name it was made for.
pub fn live(row: &str, now: i64) -> Option<([u8; 32], Option<String>)> {
    let v: serde_json::Value = serde_json::from_str(row).ok()?;
    let hash = HEXLOWER.decode(v["sha256"].as_str()?.as_bytes()).ok()?;
    let name = v["name"].as_str().map(str::to_string);
    (v["expires"].as_i64()? > now).then_some((hash.try_into().ok()?, name))
}

/// A code as typed: uppercase, without its hyphen or any space.
pub fn typed(code: &str) -> String {
    let kept = code.chars().filter(|c| *c != '-' && !c.is_whitespace());
    kept.flat_map(char::to_uppercase).collect()
}

/// The channels that may ask for a code: the strong ones but the passkey, since what a code
/// lets in is as strong as what vouched for it.
pub fn issuers(cfg: &crate::config::Config) -> Vec<&str> {
    (cfg.settings.strong_channels.iter())
        .map(String::as_str)
        .filter(|c| *c != "passkey")
        .collect()
}

/// What a pairing channel's `/pair …` text is stored as: `/pair`, whatever follows it, so a code
/// typed a little wrong never reaches state.db either. `None` keeps the text as it is.
pub fn stored(channel: &str, text: &str) -> Option<String> {
    let first = text.split_whitespace().next()?;
    (CHANNELS.contains(&channel) && first.eq_ignore_ascii_case("/pair")).then(|| "/pair".into())
}

/// The code a message on `channel` claims with, when it is a pairing channel's.
pub fn claim_on(channel: &str, text: &str) -> Option<String> {
    CHANNELS.contains(&channel).then(|| claimed(text)).flatten()
}

/// The code a message claims with, as typed: its whole text is `/pair CODE`, CODE a code's
/// shape (ten base32 characters; case, the hyphen and spaces aside). `/pair telegram` is not a
/// claim but the command, which a strong channel may send.
pub fn claimed(text: &str) -> Option<String> {
    let code = typed(text.trim().strip_prefix("/pair ")?);
    let base32 = |b: u8| b.is_ascii_uppercase() || (b'2'..=b'7').contains(&b);
    (code.len() == 10 && code.bytes().all(base32)).then_some(code)
}

/// Whether `code` (as `claimed` gives it) is the unexpired one `row` was issued for.
pub fn matches(row: &str, code: &str, now: i64) -> bool {
    live(row, now).is_some_and(|(hash, _)| bool::from(hash.ct_eq(&sha256(code.as_bytes()))))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_code_matches_typed_any_way() {
        let (shown, row) = issue(1000, None);
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
        let (shown, row) = issue(1000, Some("phone"));
        assert_eq!(live(&row, 1000).unwrap().1.as_deref(), Some("phone"));
        assert!(live(&row, 1000 + LIFE).is_none());
        let code = claimed(&format!("/pair {shown}")).unwrap();
        assert!(matches(&row, &code, 1000 + LIFE - 1));
        assert!(!matches(&row, &code, 1000 + LIFE));
    }

    #[test]
    fn a_pair_text_is_stored_cut_on_a_pairing_channel_only() {
        assert_eq!(
            stored("telegram", "/pair ABCDE-FGHJ").as_deref(),
            Some("/pair")
        );
        assert_eq!(stored("signal", " /PAIR x y").as_deref(), Some("/pair"));
        assert_eq!(stored("telegram", "/pairing is fun"), None);
        assert_eq!(stored("chat", "/pair telegram"), None);
        assert_eq!(stored("telegram", "hello"), None);
    }

    #[test]
    fn typed_drops_case_hyphens_and_spaces() {
        assert_eq!(typed(" abcde - fghjk\n"), "ABCDEFGHJK");
        assert_eq!(typed("ABCDE-FGHJK"), "ABCDEFGHJK");
    }

    #[test]
    fn claimed_needs_the_whole_text() {
        assert_eq!(claimed("  /pair abcde-fghjk "), Some("ABCDEFGHJK".into()));
        assert_eq!(claimed("/pair"), None);
        assert_eq!(claimed("/pair  "), None);
        assert_eq!(claimed("please /pair ABCDE-FGHJK"), None);
        assert_eq!(claimed("/paired ABCDE"), None);
        assert_eq!(claimed(&format!("/pair {}", "A".repeat(33))), None);
        // Only a code's shape is a claim: ten base32 characters. Anything else is `/pair` itself.
        assert_eq!(claimed("/pair telegram"), None);
        assert_eq!(claimed("/pair signal"), None);
        assert_eq!(claimed("/pair ABCDE-FGHJ1"), None);
        assert_eq!(claimed("/pair ABCDE-FGHJKL"), None);
        assert_eq!(claim_on("telegram", "/pair telegram"), None);
    }
}
