//! Passkeys for the web app (DESIGN §4.3.3): WebAuthn ES256 with user verification, checked on
//! `ring`. Pure: the caller keeps the challenges and the stored keys. Anything malformed is a
//! refusal, and a refusal's reason never holds a value from the request.

use data_encoding::{BASE64URL_NOPAD as B64, HEXLOWER};
use ring::signature::{UnparsedPublicKey, ECDSA_P256_SHA256_ASN1};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use subtle::ConstantTimeEq;

/// What `navigator.credentials.get` answers, decoded.
pub struct Assertion {
    pub id: Vec<u8>,
    pub client_data: Vec<u8>,
    pub auth_data: Vec<u8>,
    pub signature: Vec<u8>,
}

/// What `navigator.credentials.create` answers, through the browser's `getAuthenticatorData`,
/// `getPublicKey` and `getPublicKeyAlgorithm`, so no CBOR is read here.
pub struct Created {
    pub id: Vec<u8>,
    pub client_data: Vec<u8>,
    pub auth_data: Vec<u8>,
    pub public_key: Vec<u8>,
    pub alg: i64,
}

const UP: u8 = 0x01;
const UV: u8 = 0x04;
const AT: u8 = 0x40;

/// DER for "an EC public key on P-256, uncompressed", before the 65-byte point.
const SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0; 32];
    out.copy_from_slice(ring::digest::digest(&ring::digest::SHA256, data).as_ref());
    out
}

/// A card's hash as the page computes it: lowercase hex SHA-256 of the card text as sent.
pub fn card_sha256(card: &str) -> String {
    HEXLOWER.encode(&sha256(card.as_bytes()))
}

/// A base64url (no padding) field of a browser's result.
fn field(v: &Value, key: &str) -> Result<Vec<u8>, &'static str> {
    let text = v.get(key).and_then(Value::as_str).ok_or("malformed")?;
    B64.decode(text.as_bytes()).map_err(|_| "malformed")
}

pub fn assertion(v: &Value) -> Result<Assertion, &'static str> {
    Ok(Assertion {
        id: field(v, "id")?,
        client_data: field(v, "clientDataJSON")?,
        auth_data: field(v, "authenticatorData")?,
        signature: field(v, "signature")?,
    })
}

pub fn created(v: &Value) -> Result<Created, &'static str> {
    Ok(Created {
        id: field(v, "id")?,
        client_data: field(v, "clientDataJSON")?,
        auth_data: field(v, "authenticatorData")?,
        public_key: field(v, "publicKey")?,
        alg: v.get("alg").and_then(Value::as_i64).ok_or("malformed")?,
    })
}

/// An ES256 signature in DER, as an authenticator writes it.
pub fn es256(point: &[u8], msg: &[u8], der: &[u8]) -> bool {
    UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, point)
        .verify(msg, der)
        .is_ok()
}

/// The browser's own account of the ceremony. Only these four keys are read: browsers add others.
fn check_client(
    data: &[u8],
    typ: &str,
    challenge: &[u8; 32],
    host: &str,
) -> Result<(), &'static str> {
    let v: Value = serde_json::from_slice(data).unwrap_or(Value::Null);
    if v.get("type").and_then(Value::as_str) != Some(typ) {
        return Err("wrong type");
    }
    let sent = (v.get("challenge").and_then(Value::as_str))
        .and_then(|c| B64.decode(c.as_bytes()).ok())
        .unwrap_or_default();
    if !bool::from(sent.ct_eq(challenge)) {
        return Err("wrong challenge");
    }
    if v.get("origin").and_then(Value::as_str) != Some(&format!("https://{host}")) {
        return Err("wrong origin");
    }
    match v.get("crossOrigin") {
        None | Some(Value::Bool(false)) => Ok(()),
        Some(_) => Err("cross-origin"),
    }
}

/// The authenticator's flags and sign count, once it names this host and has the flags `need`.
fn check_auth(data: &[u8], host: &str, need: u8) -> Result<u32, &'static str> {
    if data.len() < 37 {
        return Err("short authenticator data");
    }
    if data[..32] != sha256(host.as_bytes()) {
        return Err("wrong rp");
    }
    let flags = data[32];
    for (flag, reason) in [
        (UP, "no user presence"),
        (UV, "no user verification"),
        (AT, "no attested credential"),
    ] {
        if need & flag != 0 && flags & flag == 0 {
            return Err(reason);
        }
    }
    Ok(u32::from_be_bytes([data[33], data[34], data[35], data[36]]))
}

/// Checks an assertion made for `challenge` on `host` under the stored P-256 `point`, and gives
/// the sign count to store.
pub fn verify(
    host: &str,
    challenge: &[u8; 32],
    point: &[u8],
    stored: u32,
    a: &Assertion,
) -> Result<u32, &'static str> {
    check_client(&a.client_data, "webauthn.get", challenge, host)?;
    let count = check_auth(&a.auth_data, host, UP | UV)?;
    let mut msg = a.auth_data.clone();
    msg.extend_from_slice(&sha256(&a.client_data));
    if !es256(point, &msg, &a.signature) {
        return Err("bad signature");
    }
    // WebAuthn §7.2 step 21: once either count is not zero, the new one must be the greater,
    // else the authenticator may be a clone. Synced passkeys keep no count: 0 both times.
    if (count != 0 || stored != 0) && count <= stored {
        return Err("sign count went back");
    }
    Ok(count)
}

/// Checks a registration made for `challenge` on `host`, and gives the P-256 point to store.
pub fn check_created(
    host: &str,
    challenge: &[u8; 32],
    c: &Created,
) -> Result<[u8; 65], &'static str> {
    check_client(&c.client_data, "webauthn.create", challenge, host)?;
    check_auth(&c.auth_data, host, UP | UV | AT)?;
    // rpIdHash 32, flags 1, count 4, AAGUID 16, then the id's length and the id.
    let len = c.auth_data.get(53..55).ok_or("no attested credential")?;
    let len = usize::from(u16::from_be_bytes([len[0], len[1]]));
    let attested = c
        .auth_data
        .get(55..55 + len)
        .ok_or("no attested credential")?;
    if attested != c.id {
        return Err("credential id differs");
    }
    if c.alg != -7 {
        return Err("not ES256");
    }
    let point = c
        .public_key
        .strip_prefix(&SPKI_PREFIX[..])
        .unwrap_or_default();
    <[u8; 65]>::try_from(point).map_err(|_| "not a P-256 key")
}

/// What a passkey signature in the app commits to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Signed {
    /// `/ok` or `/ok once` on a proposal, as stored (`Store::proposal_digest`) and as shown.
    Decide {
        id: i64,
        verb: String,
        digest: [u8; 32],
        card_sha256: String,
    },
    /// A setting change, and the value it changes from.
    Set {
        key: String,
        value: String,
        until: Option<String>,
        current: Option<String>,
    },
    /// Taking the agent's desktop over. A signature for a card or a setting is none for this.
    Desktop,
}

/// The bytes whose SHA-256 is the WebAuthn challenge: a JSON array, so the order is fixed.
pub fn preimage(s: &Signed, nonce: &[u8; 16]) -> Vec<u8> {
    let nonce = HEXLOWER.encode(nonce);
    let v = match s {
        Signed::Decide {
            id,
            verb,
            digest,
            card_sha256,
        } => json!([
            "zephyr-web/1",
            "decide",
            id,
            verb,
            HEXLOWER.encode(digest),
            card_sha256,
            nonce
        ]),
        Signed::Set {
            key,
            value,
            until,
            current,
        } => json!(["zephyr-web/1", "set", key, value, until, current, nonce]),
        Signed::Desktop => json!(["zephyr-web/1", "desktop", "take", nonce]),
    };
    v.to_string().into_bytes()
}

struct Pending<T> {
    id: String,
    owner: [u8; 32],
    what: T,
    challenge: [u8; 32],
    at: Instant,
}

/// Challenges waiting for their signature, in memory: each lives `ttl`, is used once, and
/// belongs to one owner (a session's hash, or zeroes before sign-in). At most `cap` wait.
pub struct Challenges<T> {
    ttl: Duration,
    cap: usize,
    inner: Mutex<VecDeque<Pending<T>>>,
}

impl<T> Challenges<T> {
    pub fn new(ttl: Duration, cap: usize) -> Self {
        Self {
            ttl,
            cap,
            inner: Mutex::new(VecDeque::new()),
        }
    }

    /// Keeps a challenge under a random name. Past `cap`, the oldest goes.
    pub fn issue(&self, owner: [u8; 32], what: T, challenge: [u8; 32]) -> String {
        let mut waiting = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if waiting.len() >= self.cap {
            waiting.pop_front();
        }
        self.keep(&mut waiting, owner, what, challenge)
    }

    /// Like `issue`, but turns the new one away (`None`) rather than drop one that waits: for
    /// challenges anyone may ask for, so a flood of asking cannot push out the owner's.
    pub fn try_issue(&self, owner: [u8; 32], what: T, challenge: [u8; 32]) -> Option<String> {
        let mut waiting = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        waiting.retain(|p| p.at.elapsed() < self.ttl);
        (waiting.len() < self.cap).then(|| self.keep(&mut waiting, owner, what, challenge))
    }

    fn keep(
        &self,
        waiting: &mut VecDeque<Pending<T>>,
        owner: [u8; 32],
        what: T,
        challenge: [u8; 32],
    ) -> String {
        // 128 random bits: the name tells nothing of how many were issued or to whom.
        let id = HEXLOWER.encode(&crate::web::random::<16>());
        waiting.push_back(Pending {
            id: id.clone(),
            owner,
            what,
            challenge,
            at: Instant::now(),
        });
        id
    }

    /// Takes its owner's challenge out, whether or not what follows verifies. `None` for a used
    /// one or one past its `ttl`, and for another owner's, which is left where it is.
    pub fn take(&self, id: &str, owner: [u8; 32]) -> Option<(T, [u8; 32])> {
        let mut waiting = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        let at = (waiting.iter()).position(|p| p.id == id && bool::from(p.owner.ct_eq(&owner)))?;
        let p = waiting.remove(at)?;
        (p.at.elapsed() < self.ttl).then_some((p.what, p.challenge))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, KeyPair, ECDSA_P256_SHA256_ASN1_SIGNING};

    const HOST: &str = "zephyr.test";
    const CHALLENGE: [u8; 32] = [7; 32];
    const UP: u8 = 0x01;
    const UV: u8 = 0x04;
    const AT: u8 = 0x40;

    fn hex(s: &str) -> Vec<u8> {
        data_encoding::HEXUPPER_PERMISSIVE
            .decode(s.as_bytes())
            .unwrap()
    }

    /// A software authenticator: a P-256 key that signs what a platform one would.
    struct Soft {
        key: EcdsaKeyPair,
        rng: SystemRandom,
    }

    impl Soft {
        fn new() -> Self {
            let rng = SystemRandom::new();
            let pkcs8 =
                EcdsaKeyPair::generate_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, &rng).unwrap();
            let key =
                EcdsaKeyPair::from_pkcs8(&ECDSA_P256_SHA256_ASN1_SIGNING, pkcs8.as_ref(), &rng)
                    .unwrap();
            Self { key, rng }
        }

        fn point(&self) -> Vec<u8> {
            self.key.public_key().as_ref().to_vec()
        }

        fn sign(&self, client_data: Vec<u8>, auth_data: Vec<u8>) -> Assertion {
            let mut msg = auth_data.clone();
            msg.extend_from_slice(sha256(&client_data).as_ref());
            let signature = self.key.sign(&self.rng, &msg).unwrap().as_ref().to_vec();
            Assertion {
                id: vec![1, 2, 3],
                client_data,
                auth_data,
                signature,
            }
        }
    }

    fn client_data(typ: &str, challenge: &[u8], origin: &str) -> Vec<u8> {
        let challenge = B64.encode(challenge);
        serde_json::json!({"type": typ, "challenge": challenge, "origin": origin})
            .to_string()
            .into_bytes()
    }

    fn auth_data(host: &str, flags: u8, count: u32) -> Vec<u8> {
        let mut out = sha256(host.as_bytes()).to_vec();
        out.push(flags);
        out.extend_from_slice(&count.to_be_bytes());
        out
    }

    fn good(soft: &Soft, count: u32) -> Assertion {
        soft.sign(
            client_data("webauthn.get", &CHALLENGE, "https://zephyr.test"),
            auth_data(HOST, UP | UV, count),
        )
    }

    #[test]
    fn the_rfc_6979_p256_sha256_signature_verifies() {
        let mut point = vec![4];
        point.extend(hex(
            "60FED4BA255A9D31C961EB74C6356D68C049B8923B61FA6CE669622E60F29FB6",
        ));
        point.extend(hex(
            "7903FE1008B8BC99A41AE9E95628BC64F2F1B20C2D7E9F5177A3C294D4462299",
        ));
        let der = hex(
            "3046022100EFD48B2AACB6A8FD1140DD9CD45E81D69D2C877B56AAF991C34D0EA84EAF3716\
             022100F7CB1C942D657C41D436C7A1B6E29F65F3E900DBB9AFF4064DC4AB2F843ACDA8",
        );
        assert!(es256(&point, b"sample", &der));
        assert!(!es256(&point, b"test", &der));
    }

    #[test]
    fn verify_accepts_a_good_assertion_and_returns_its_count() {
        let soft = Soft::new();
        let point = soft.point();
        assert_eq!(verify(HOST, &CHALLENGE, &point, 4, &good(&soft, 5)), Ok(5));
        assert_eq!(
            verify(HOST, &CHALLENGE, &point, 0, &good(&soft, 0)),
            Ok(0),
            "a synced passkey keeps no count"
        );
        assert_eq!(verify(HOST, &CHALLENGE, &point, 0, &good(&soft, 1)), Ok(1));
        // WebAuthn §7.2: once either count is not zero, the new one must be the greater.
        for (stored, new) in [(9, 0), (9, 9), (9, 8), (1, 0)] {
            let refused = verify(HOST, &CHALLENGE, &point, stored, &good(&soft, new));
            assert_eq!(refused, Err("sign count went back"), "{stored} then {new}");
        }
    }

    #[test]
    fn verify_refuses_each_broken_field() {
        let soft = Soft::new();
        let get = |typ: &str, challenge: &[u8], origin: &str| client_data(typ, challenge, origin);
        let ok_client = || get("webauthn.get", &CHALLENGE, "https://zephyr.test");
        let ok_auth = || auth_data(HOST, UP | UV, 5);
        let mut cross: serde_json::Value = serde_json::from_slice(&ok_client()).unwrap();
        cross["crossOrigin"] = true.into();
        let mut flipped = good(&soft, 5);
        *flipped.signature.last_mut().unwrap() ^= 1;
        let cases = [
            (
                "wrong type",
                soft.sign(
                    get("webauthn.create", &CHALLENGE, "https://zephyr.test"),
                    ok_auth(),
                ),
            ),
            (
                "wrong challenge",
                soft.sign(
                    get("webauthn.get", &[8; 32], "https://zephyr.test"),
                    ok_auth(),
                ),
            ),
            (
                "wrong origin",
                soft.sign(
                    get("webauthn.get", &CHALLENGE, "https://evil.test"),
                    ok_auth(),
                ),
            ),
            (
                "cross-origin",
                soft.sign(cross.to_string().into_bytes(), ok_auth()),
            ),
            (
                "short authenticator data",
                soft.sign(ok_client(), ok_auth()[..36].to_vec()),
            ),
            (
                "wrong rp",
                soft.sign(ok_client(), auth_data("other.test", UP | UV, 5)),
            ),
            (
                "no user presence",
                soft.sign(ok_client(), auth_data(HOST, UV, 5)),
            ),
            (
                "no user verification",
                soft.sign(ok_client(), auth_data(HOST, UP, 5)),
            ),
            ("bad signature", flipped),
            ("sign count went back", good(&soft, 4)),
        ];
        for (reason, assertion) in cases {
            assert_eq!(
                verify(HOST, &CHALLENGE, &soft.point(), 4, &assertion),
                Err(reason)
            );
        }
        // Another key's signature, and client data that is not JSON at all.
        assert_eq!(
            verify(HOST, &CHALLENGE, &Soft::new().point(), 4, &good(&soft, 5)),
            Err("bad signature")
        );
        let junk = soft.sign(b"not json".to_vec(), ok_auth());
        assert_eq!(
            verify(HOST, &CHALLENGE, &soft.point(), 4, &junk),
            Err("wrong type")
        );
    }

    #[test]
    fn chromes_extra_client_data_key_and_cross_origin_false_are_accepted() {
        let soft = Soft::new();
        let client = format!(
            r#"{{"type":"webauthn.get","challenge":"{}","origin":"https://zephyr.test","crossOrigin":false,"other_keys_can_be_added_here":"do not compare clientDataJSON against a template. See https://goo.gl/yabPex"}}"#,
            B64.encode(&CHALLENGE)
        );
        let a = soft.sign(client.into_bytes(), auth_data(HOST, UP | UV, 1));
        assert_eq!(verify(HOST, &CHALLENGE, &soft.point(), 0, &a), Ok(1));
    }

    const SPKI: &str = "3059301306072A8648CE3D020106082A8648CE3D030107034200";

    fn creation(soft: &Soft, flags: u8, attested: &[u8]) -> Created {
        let mut auth = auth_data(HOST, flags, 0);
        auth.extend_from_slice(&[0; 16]); // the AAGUID
        auth.extend_from_slice(&(attested.len() as u16).to_be_bytes());
        auth.extend_from_slice(attested);
        auth.extend_from_slice(b"cose key, never read");
        let mut public_key = hex(SPKI);
        public_key.extend(soft.point());
        Created {
            id: vec![1, 2, 3],
            client_data: client_data("webauthn.create", &CHALLENGE, "https://zephyr.test"),
            auth_data: auth,
            public_key,
            alg: -7,
        }
    }

    #[test]
    fn check_created_reads_the_point_and_refuses_bad_registrations() {
        let soft = Soft::new();
        let good = || creation(&soft, UP | UV | AT, &[1, 2, 3]);
        assert_eq!(
            check_created(HOST, &CHALLENGE, &good()).unwrap().to_vec(),
            soft.point()
        );

        let check = |c: Created| check_created(HOST, &CHALLENGE, &c).unwrap_err();
        assert_eq!(
            check(creation(&soft, UP | UV, &[1, 2, 3])),
            "no attested credential"
        );
        assert_eq!(
            check(creation(&soft, UP | UV | AT, &[1, 2, 4])),
            "credential id differs"
        );
        let mut c = good();
        c.auth_data.truncate(54);
        assert_eq!(check(c), "no attested credential");
        let mut c = good();
        c.alg = -257;
        assert_eq!(check(c), "not ES256");
        let mut c = good();
        c.public_key[3] ^= 1;
        assert_eq!(check(c), "not a P-256 key");
        let mut c = good();
        c.public_key.pop();
        assert_eq!(check(c), "not a P-256 key");
        let mut c = good();
        c.client_data = client_data("webauthn.get", &CHALLENGE, "https://zephyr.test");
        assert_eq!(check(c), "wrong type");
        assert_eq!(
            check(creation(&soft, UV | AT, &[1, 2, 3])),
            "no user presence"
        );
    }

    #[test]
    fn browser_results_are_read_from_base64url_and_malformed_ones_refused() {
        let v = serde_json::json!({"id": "AQID", "clientDataJSON": "e30", "authenticatorData": "AA",
            "signature": "_w", "publicKey": "BA", "alg": -7, "userHandle": "ignored"});
        let a = assertion(&v).unwrap();
        assert_eq!(
            (a.id, a.client_data, a.signature),
            (vec![1, 2, 3], b"{}".to_vec(), vec![255])
        );
        let c = created(&v).unwrap();
        assert_eq!((c.public_key, c.alg), (vec![4], -7));
        for broken in [
            serde_json::json!({"id": "AQID", "clientDataJSON": "e30", "authenticatorData": "AA"}),
            serde_json::json!({"id": "AQID=", "clientDataJSON": "e30", "authenticatorData": "AA",
                "signature": "_w"}),
            serde_json::json!({"id": 5, "clientDataJSON": "e30", "authenticatorData": "AA",
                "signature": "_w"}),
            serde_json::json!("x"),
        ] {
            assert!(
                assertion(&broken).is_err() && created(&broken).is_err(),
                "{broken}"
            );
        }
    }

    #[test]
    fn preimages_that_differ_in_any_field_give_different_challenges() {
        let decide = |id, verb: &str, digest, card: &str| Signed::Decide {
            id,
            verb: verb.into(),
            digest,
            card_sha256: card.into(),
        };
        let set =
            |key: &str, value: &str, until: Option<&str>, current: Option<&str>| Signed::Set {
                key: key.into(),
                value: value.into(),
                until: until.map(Into::into),
                current: current.map(Into::into),
            };
        let cases = [
            (decide(7, "ok", [1; 32], "aa"), [0; 16]),
            (decide(8, "ok", [1; 32], "aa"), [0; 16]),
            (decide(7, "once", [1; 32], "aa"), [0; 16]),
            (decide(7, "ok", [2; 32], "aa"), [0; 16]),
            (decide(7, "ok", [1; 32], "ab"), [0; 16]),
            (decide(7, "ok", [1; 32], "aa"), [1; 16]),
            (set("k", "v", None, None), [0; 16]),
            (set("j", "v", None, None), [0; 16]),
            (set("k", "w", None, None), [0; 16]),
            (set("k", "v", Some("2h"), None), [0; 16]),
            (set("k", "v", None, Some("2h")), [0; 16]),
            (set("k", "v", None, Some("null")), [0; 16]),
            (set("k", "v", None, None), [1; 16]),
            (Signed::Desktop, [0; 16]),
            (Signed::Desktop, [1; 16]),
            // A setting named to look like the takeover is still a `set`.
            (set("desktop", "take", None, None), [0; 16]),
        ];
        let hashes: std::collections::HashSet<_> = cases
            .iter()
            .map(|(s, nonce)| sha256(&preimage(s, nonce)))
            .collect();
        assert_eq!(hashes.len(), cases.len());
        assert_eq!(
            String::from_utf8(preimage(&cases[0].0, &[0xab; 16])).unwrap(),
            format!(
                r#"["zephyr-web/1","decide",7,"ok","{}","aa","{}"]"#,
                "01".repeat(32),
                "ab".repeat(16)
            )
        );
        assert_eq!(
            String::from_utf8(preimage(&cases[9].0, &[0; 16])).unwrap(),
            format!(
                r#"["zephyr-web/1","set","k","v","2h",null,"{}"]"#,
                "00".repeat(16)
            )
        );
    }

    #[test]
    fn the_takeover_has_a_preimage_of_its_own() {
        assert_eq!(
            String::from_utf8(preimage(&Signed::Desktop, &[0xab; 16])).unwrap(),
            format!(r#"["zephyr-web/1","desktop","take","{}"]"#, "ab".repeat(16))
        );
    }

    #[test]
    fn a_challenge_is_taken_once_by_its_owner_within_its_ttl() {
        let minute = Duration::from_secs(60);
        let q = Challenges::new(minute, 16);
        let (me, other) = ([1; 32], [2; 32]);
        let id = q.issue(me, "a", [9; 32]);
        assert_eq!(q.take(&id, me), Some(("a", [9; 32])));
        assert_eq!(q.take(&id, me), None, "used once");

        // Another owner neither gets it nor uses it up.
        let id = q.issue(me, "b", [9; 32]);
        assert_eq!(q.take(&id, other), None, "not its owner");
        assert_eq!(
            q.take(&id, me),
            Some(("b", [9; 32])),
            "and it is still its owner's"
        );
        assert_eq!(q.take("c99", me), None);

        // Ids are random, so one session cannot name another's challenge: 128 bits, hex.
        let ids: Vec<String> = (0..8).map(|n| q.issue(me, "x", [n; 32])).collect();
        let distinct: std::collections::HashSet<&String> = ids.iter().collect();
        assert_eq!(distinct.len(), 8);
        assert!(ids.iter().all(|id| id.len() == 32), "{ids:?}");

        // `try_issue` turns a new one away rather than drop one that waits.
        let full = Challenges::new(minute, 2);
        let first = full.try_issue(me, 1, [9; 32]).unwrap();
        assert!(full.try_issue(me, 2, [9; 32]).is_some());
        assert_eq!(full.try_issue(me, 3, [9; 32]), None);
        assert_eq!(full.take(&first, me), Some((1, [9; 32])));
        assert!(
            full.try_issue(me, 4, [9; 32]).is_some(),
            "a place came free"
        );
        // One past its ttl holds no place.
        let stale = Challenges::new(Duration::ZERO, 1);
        assert!(stale.try_issue(me, 1, [9; 32]).is_some());
        assert!(stale.try_issue(me, 2, [9; 32]).is_some());

        let stale = Challenges::new(Duration::ZERO, 16);
        let id = stale.issue(me, "c", [9; 32]);
        assert_eq!(stale.take(&id, me), None, "past its ttl");

        let q = Challenges::new(minute, 16);
        let ids: Vec<String> = (0..17).map(|n| q.issue(me, n, [9; 32])).collect();
        assert_eq!(q.take(&ids[0], me), None, "the 17th evicted the first");
        assert_eq!(q.take(&ids[1], me), Some((1, [9; 32])));
        assert_eq!(q.take(&ids[16], me), Some((16, [9; 32])));
    }
}
