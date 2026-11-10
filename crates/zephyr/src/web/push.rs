//! Web Push for the web app (DESIGN §4.3.3): the aes128gcm message encryption of RFC 8291, the
//! VAPID token of RFC 8292 and the push services a subscription may name, on `ring`.

use data_encoding::BASE64URL_NOPAD as B64;
use ring::aead::{Aad, LessSafeKey, Nonce, UnboundKey, AES_128_GCM};
use ring::hkdf::{self, HKDF_SHA256};
use ring::signature::EcdsaKeyPair;
use serde_json::{json, Value};

/// The header: salt 16, record size 4, key length 1, the sender's 65-byte point.
const HEADER: usize = 86;
/// One record's size, and the most a push service takes.
const RECORD: usize = 4096;
/// The most one push carries: the record less its header, the delimiter and the 16-byte tag.
pub const MAX_PLAINTEXT: usize = RECORD - HEADER - 17;

struct Len(usize);

impl hkdf::KeyType for Len {
    fn len(&self) -> usize {
        self.0
    }
}

fn expand<const N: usize>(prk: &hkdf::Prk, info: &[&[u8]]) -> [u8; N] {
    let mut out = [0; N];
    // Only a length over 255 hashes fails, and these are 32, 16 and 12 bytes.
    (prk.expand(info, Len(N)).and_then(|okm| okm.fill(&mut out)))
        .expect("an HKDF-SHA256 output of at most 32 bytes");
    out
}

/// The content key and nonce of RFC 8291 §3.4, from the ECDH secret both sides share.
fn keys(
    ecdh_secret: &[u8],
    as_public: &[u8],
    ua_public: &[u8],
    auth: &[u8],
    salt: &[u8],
) -> (LessSafeKey, Nonce) {
    let prk_key = hkdf::Salt::new(HKDF_SHA256, auth).extract(ecdh_secret);
    let ikm: [u8; 32] = expand(&prk_key, &[b"WebPush: info\0", ua_public, as_public]);
    let prk = hkdf::Salt::new(HKDF_SHA256, salt).extract(&ikm);
    let cek: [u8; 16] = expand(&prk, &[b"Content-Encoding: aes128gcm\0"]);
    let nonce: [u8; 12] = expand(&prk, &[b"Content-Encoding: nonce\0"]);
    let key = UnboundKey::new(&AES_128_GCM, &cek).expect("a 16-byte AES-128 key");
    (LessSafeKey::new(key), Nonce::assume_unique_for_key(nonce))
}

/// One aes128gcm record for a subscription: the header, then `plaintext` sealed. `salt` must be
/// fresh for each message, and `ecdh_secret` come from a fresh key pair whose point is `as_public`.
///
/// Panics when `plaintext` is longer than `MAX_PLAINTEXT`; `card` and `reply` stay far below it.
pub fn encrypt(
    ecdh_secret: &[u8],
    as_public: &[u8; 65],
    ua_public: &[u8],
    auth: &[u8],
    salt: &[u8; 16],
    plaintext: &[u8],
) -> Vec<u8> {
    assert!(plaintext.len() <= MAX_PLAINTEXT, "a push holds one record");
    let (key, nonce) = keys(ecdh_secret, as_public, ua_public, auth, salt);
    let mut record = plaintext.to_vec();
    record.push(2); // the last record's delimiter
    key.seal_in_place_append_tag(nonce, Aad::empty(), &mut record)
        .expect("a record far below AES-GCM's limit");
    let mut out = Vec::with_capacity(HEADER + record.len());
    out.extend_from_slice(salt);
    out.extend_from_slice(&(RECORD as u32).to_be_bytes());
    out.push(65);
    out.extend_from_slice(as_public);
    out.extend_from_slice(&record);
    out
}

/// What a browser does with `encrypt`'s output: the tests' receiver.
pub fn decrypt(ecdh_secret: &[u8], ua_public: &[u8], auth: &[u8], body: &[u8]) -> Option<Vec<u8>> {
    let (salt, rest) = body.split_at_checked(16)?;
    let (head, rest) = rest.split_at_checked(5)?;
    let (as_public, sealed) = rest.split_at_checked(usize::from(head[4]))?;
    let (key, nonce) = keys(ecdh_secret, as_public, ua_public, auth, salt);
    let mut record = sealed.to_vec();
    let plain = key.open_in_place(nonce, Aad::empty(), &mut record).ok()?;
    plain.strip_suffix(&[2]).map(<[u8]>::to_vec)
}

/// The VAPID token for one push service: an ES256 JWT naming the endpoint's origin, good for 12
/// hours. `key` must be an `ECDSA_P256_SHA256_FIXED_SIGNING` pair: JOSE takes r‖s, not DER.
pub fn vapid_jwt(key: &EcdsaKeyPair, endpoint: &url::Url, host: &str, now: i64) -> String {
    let claims = json!({
        "aud": endpoint.origin().ascii_serialization(),
        "exp": now + 43_200,
        "sub": format!("https://{host}"),
    });
    // {"typ":"JWT","alg":"ES256"}
    let signed = format!(
        "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.{}",
        B64.encode(claims.to_string().as_bytes())
    );
    let signature = key
        .sign(&ring::rand::SystemRandom::new(), signed.as_bytes())
        .expect("the system's random source");
    format!("{signed}.{}", B64.encode(signature.as_ref()))
}

/// A subscription's endpoint, when it is one of the push services (or, for the tests, an origin
/// in `extra`): the daemon posts nowhere else.
pub fn allowed_endpoint(endpoint: &str, extra: &[String]) -> Option<url::Url> {
    let url = url::Url::parse(endpoint).ok()?;
    if !url.username().is_empty() || url.password().is_some() {
        return None;
    }
    let host = url.host_str()?;
    let service = url.scheme() == "https"
        && url.port().is_none()
        && (host == "fcm.googleapis.com"
            || [
                ".push.apple.com",
                ".push.services.mozilla.com",
                ".notify.windows.com",
            ]
            .iter()
            .any(|suffix| host.ends_with(suffix)));
    let origin = url.origin().ascii_serialization();
    (service || extra.contains(&origin)).then_some(url)
}

fn cut(text: &str, chars: usize) -> Option<&str> {
    text.char_indices().nth(chars).map(|(at, _)| &text[..at])
}

/// A new card's notification. `body` is the caller's, already scrubbed; never the agent's reason.
pub fn card(id: i64, body: &str, quick: bool) -> Value {
    let body = match cut(body, 200) {
        Some(head) => format!("{head}…"),
        None => body.to_string(),
    };
    json!({"kind": "card", "id": id, "title": format!("zephyr: approve #{id}"), "body": body,
        "quick": quick})
}

/// A reply's notification: no more of it than a Discord DM would show.
pub fn reply(body: &str) -> Value {
    json!({"kind": "reply", "body": cut(body, 120).unwrap_or(body)})
}

/// The VAPID key, `<state>/web/vapid.pk8` (0600 in a 0700 folder, the daemon's alone). `make`
/// writes a new one when the file is missing; a file that does not load is an error either way,
/// never replaced, since every subscription is bound to the key it was made with.
pub fn vapid(cfg: &crate::config::Config, make: bool) -> anyhow::Result<EcdsaKeyPair> {
    use anyhow::Context;
    use std::io::Write;
    use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
    let alg = &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
    let rng = ring::rand::SystemRandom::new();
    let dir = cfg.state_dir().join("web");
    let path = dir.join("vapid.pk8");
    if make && !path.exists() {
        let made = EcdsaKeyPair::generate_pkcs8(alg, &rng)
            .map_err(|_| anyhow::anyhow!("the system's random source failed"))?;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)?;
        let mut file = (std::fs::OpenOptions::new().write(true).create_new(true))
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("writing {}", path.display()))?;
        file.write_all(made.as_ref())?;
        file.sync_all()?;
    }
    let pkcs8 = std::fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    EcdsaKeyPair::from_pkcs8(alg, &pkcs8, &rng)
        .map_err(|_| anyhow::anyhow!("{} is not a P-256 key", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use data_encoding::BASE64URL_NOPAD as B64;
    use ring::signature::{KeyPair, UnparsedPublicKey, ECDSA_P256_SHA256_FIXED};

    fn b64(s: &str) -> Vec<u8> {
        B64.decode(s.as_bytes()).unwrap()
    }

    // RFC 8291 Appendix A.
    const ECDH_SECRET: &str = "kyrL1jIIOHEzg3sM2ZWRHDRB62YACZhhSlknJ672kSs";
    const AUTH: &str = "BTBZMqHH6r4Tts7J_aSIgg";
    const SALT: &str = "DGv6ra1nlYgDCS1FRnbzlw";
    const AS_PUBLIC: &str =
        "BP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A8";
    const UA_PUBLIC: &str =
        "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4";

    fn rfc_encrypt(plaintext: &[u8]) -> Vec<u8> {
        encrypt(
            &b64(ECDH_SECRET),
            &b64(AS_PUBLIC).try_into().unwrap(),
            &b64(UA_PUBLIC),
            &b64(AUTH),
            &b64(SALT).try_into().unwrap(),
            plaintext,
        )
    }

    #[test]
    fn encrypt_matches_rfc_8291_appendix_a() {
        let plaintext = b"When I grow up, I want to be a watermelon";
        let body = rfc_encrypt(plaintext);
        assert_eq!(
            B64.encode(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
        let open = |body: &[u8]| decrypt(&b64(ECDH_SECRET), &b64(UA_PUBLIC), &b64(AUTH), body);
        assert_eq!(open(&body).as_deref(), Some(&plaintext[..]));
        let mut bent = body.clone();
        *bent.last_mut().unwrap() ^= 1;
        assert_eq!(open(&bent), None, "a changed tag");
        assert_eq!(open(&body[..80]), None, "a cut header");
        assert_eq!(
            decrypt(&b64(ECDH_SECRET), &b64(UA_PUBLIC), &[0; 16], &body),
            None
        );
    }

    /// The claims of an ES256 JWT whose signature (r‖s, as JOSE writes it) verifies under `point`.
    fn jwt_claims(token: &str, point: &[u8]) -> Option<serde_json::Value> {
        let (signed, signature) = token.rsplit_once('.')?;
        let signature = B64.decode(signature.as_bytes()).ok()?;
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_FIXED, point)
            .verify(signed.as_bytes(), &signature)
            .ok()?;
        let claims = B64.decode(signed.split_once('.')?.1.as_bytes()).ok()?;
        serde_json::from_slice(&claims).ok()
    }

    #[test]
    fn the_vapid_jwt_verifies_and_carries_the_claims() {
        let rng = ring::rand::SystemRandom::new();
        let alg = &ring::signature::ECDSA_P256_SHA256_FIXED_SIGNING;
        let pkcs8 = EcdsaKeyPair::generate_pkcs8(alg, &rng).unwrap();
        let key = EcdsaKeyPair::from_pkcs8(alg, pkcs8.as_ref(), &rng).unwrap();
        let endpoint = url::Url::parse("https://fcm.googleapis.com/fcm/send/abc").unwrap();
        let now = 1_700_000_000;
        let jwt = vapid_jwt(&key, &endpoint, "zephyr.test", now);
        assert!(
            jwt.starts_with("eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9."),
            "{jwt}"
        );
        let claims = jwt_claims(&jwt, key.public_key().as_ref()).expect("our token verifies");
        assert_eq!(
            claims,
            serde_json::json!({"aud": "https://fcm.googleapis.com", "exp": now + 43_200,
                "sub": "https://zephyr.test"})
        );
        assert!(
            jwt_claims(&jwt, &b64(AS_PUBLIC)).is_none(),
            "not under another key"
        );
        let local = url::Url::parse("http://127.0.0.1:9000/p/1").unwrap();
        let claims = jwt_claims(&vapid_jwt(&key, &local, "h", 0), key.public_key().as_ref());
        assert_eq!(claims.unwrap()["aud"], "http://127.0.0.1:9000");

        // RFC 8292 §2.4's example, which proves the helper reads the fixed form.
        let rfc = "eyJ0eXAiOiJKV1QiLCJhbGciOiJFUzI1NiJ9.eyJhdWQiOiJodHRwczovL3B1c2guZXhhbXBsZS5uZXQiLCJleHAiOjE0NTM1MjM3NjgsInN1YiI6Im1haWx0bzpwdXNoQGV4YW1wbGUuY29tIn0.i3CYb7t4xfxCDquptFOepC9GAu_HLGkMlMuCGSK2rpiUfnK9ojFwDXb1JrErtmysazNjjvW2L9OkSSHzvoD1oA";
        let k = b64("BA1Hxzyi1RUM1b5wjxsn7nGxAszw2u61m164i3MrAIxHF6YK5h4SDYic-dRuU_RCPCfA5aq9ojSwk5Y2EmClBPs");
        assert_eq!(
            jwt_claims(rfc, &k).expect("the RFC's token verifies"),
            serde_json::json!({"aud": "https://push.example.net", "exp": 1_453_523_768,
                "sub": "mailto:push@example.com"})
        );
    }

    #[test]
    fn the_vapid_key_is_made_once_private_and_loads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_dir("web-vapid");
        let cfg: crate::config::Config =
            toml::from_str(&format!("rpi = 'r'\nhome = {dir:?}\n")).unwrap();
        assert!(vapid(&cfg, false).is_err(), "not made by a read");
        let made = vapid(&cfg, true).unwrap();
        let again = vapid(&cfg, false).unwrap();
        assert_eq!(made.public_key().as_ref(), again.public_key().as_ref());
        assert_eq!(
            vapid(&cfg, true).unwrap().public_key().as_ref(),
            made.public_key().as_ref()
        );
        let mode = |p: &std::path::Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let path = dir.join("web/vapid.pk8");
        assert_eq!((mode(&dir.join("web")), mode(&path)), (0o700, 0o600));
        std::fs::write(&path, b"junk").unwrap();
        assert!(
            vapid(&cfg, true).is_err(),
            "a broken key is not replaced silently"
        );
    }

    #[test]
    fn the_allowlist_takes_push_services_only() {
        for ok in [
            "https://fcm.googleapis.com/fcm/send/x",
            "https://web.push.apple.com/Q",
            "https://updates.push.services.mozilla.com/wpush/v2/x",
            "https://db5p.notify.windows.com/w/?token=x",
            "https://fcm.googleapis.com:443/x",
        ] {
            assert!(allowed_endpoint(ok, &[]).is_some(), "{ok}");
        }
        for no in [
            "https://fcm.googleapis.com.evil.test/x",
            "http://fcm.googleapis.com/x",
            "https://u@fcm.googleapis.com/x",
            "https://u:p@fcm.googleapis.com/x",
            "https://fcm.googleapis.com:8443/x",
            "https://push.apple.com.evil.test/x",
            "https://evilpush.apple.com/x",
            "https://x.fcm.googleapis.com/x",
            "http://127.0.0.1:9/p",
            "not a url",
        ] {
            assert!(allowed_endpoint(no, &[]).is_none(), "{no}");
        }
        let extra = ["http://127.0.0.1:9".to_string()];
        assert!(allowed_endpoint("http://127.0.0.1:9/p", &extra).is_some());
        assert!(allowed_endpoint("http://127.0.0.1:10/p", &extra).is_none());
        assert!(allowed_endpoint("http://u@127.0.0.1:9/p", &extra).is_none());
    }

    #[test]
    fn a_long_korean_card_still_fits_one_push() {
        let payload = card(7, &"가".repeat(1500), false);
        assert_eq!(payload["kind"], "card");
        assert_eq!(payload["id"], 7);
        assert_eq!(payload["title"], "zephyr: approve #7");
        assert_eq!(payload["quick"], false);
        let body = payload["body"].as_str().unwrap();
        assert!(
            body.ends_with('…') && body.chars().count() == 201,
            "{}",
            body.chars().count()
        );
        let bytes = serde_json::to_vec(&payload).unwrap();
        assert!(bytes.len() <= MAX_PLAINTEXT, "{}", bytes.len());
        let sealed = rfc_encrypt(&bytes);
        assert!(sealed.len() <= 4096, "{}", sealed.len());
        assert_eq!(
            rfc_encrypt(&vec![b'x'; MAX_PLAINTEXT]).len(),
            4096,
            "the largest record"
        );

        assert_eq!(card(1, "short", true)["body"], "short");
        let r = reply(&"x".repeat(500));
        assert_eq!(
            (r["kind"].as_str(), r["body"].as_str().unwrap().len()),
            (Some("reply"), 120)
        );
        assert_eq!(reply("hi")["body"], "hi");
    }
}
