//! The token broker's certificate authority (DESIGN §4.21): a CA that exists only in this
//! container, so the proxy can decrypt the hosts a token is bound to. Its key is root's and never
//! leaves `<state>/ca/`; it is made and renewed only at a container start (`supervise`), before
//! any process of the agent's exists, so nothing ever runs with a trust file older than the CA.

use anyhow::Context;
use rcgen::{
    BasicConstraints, CertificateParams, DistinguishedName, DnType, IsCa, Issuer, KeyPair,
    KeyUsagePurpose,
};
use rustls::pki_types::{PrivateKeyDer, PrivatePkcs8KeyDer};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

/// How long a CA is valid, and under how many days left a container start makes a new one.
pub const DAYS: i64 = 398;
pub const RENEW_UNDER: i64 = 180;
const LEAF_DAYS: i64 = 30;
const DAY: i64 = 24 * 60 * 60;
const SUBJECT: &str = "zephyr token broker (this container only)";

/// `<state>/ca`: root's, 0700.
pub fn dir(state: &Path) -> PathBuf {
    state.join("ca")
}

/// Sets the days a certificate is valid: from midnight UTC of `from`'s day to that of `to`'s.
fn valid(params: &mut CertificateParams, from: i64, to: i64) -> anyhow::Result<()> {
    let day = |unix: i64| {
        let stamp = jiff::Timestamp::from_second(unix).context("a time out of range")?;
        let date = stamp.to_zoned(jiff::tz::TimeZone::UTC).date();
        let (month, day) = (date.month() as u8, date.day() as u8);
        anyhow::Ok(rcgen::date_time_ymd(date.year().into(), month, day))
    };
    params.not_before = day(from)?;
    params.not_after = day(to)?;
    Ok(())
}

/// What the CA's certificate says, without its dates: also what signs a leaf.
fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    params.distinguished_name = DistinguishedName::new();
    params.distinguished_name.push(DnType::CommonName, SUBJECT);
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// When the CA in `state` ends, as unix seconds: written beside the certificate when it is
/// made. Missing or unreadable reads as no CA.
/// shortcut: the date is read from this file, not parsed out of the certificate; both are
/// root's and written together.
fn expires(state: &Path) -> Option<i64> {
    let dir = dir(state);
    let text = std::fs::read_to_string(dir.join("expires")).ok()?;
    (dir.join("ca.key").exists() && dir.join("ca.pem").exists())
        .then(|| text.trim().parse().ok())?
}

/// The days the CA has left.
pub fn days_left(state: &Path, now: i64) -> anyhow::Result<i64> {
    let expires = expires(state).context("there is no CA")?;
    Ok((expires - now).div_euclid(DAY))
}

/// The CA's certificate, for the agent's tools to trust.
pub fn pem(state: &Path) -> anyhow::Result<String> {
    std::fs::read_to_string(dir(state).join("ca.pem")).context("reading the CA's certificate")
}

/// Makes the CA when there is none or it has under `RENEW_UNDER` days left, replacing the old
/// key. Returns whether it made one. Only `supervise` calls this, at a container start.
pub fn ensure(state: &Path, now: i64) -> anyhow::Result<bool> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    if expires(state).is_some_and(|e| e - now >= RENEW_UNDER * DAY) {
        return Ok(false);
    }
    let dir = dir(state);
    if !dir.exists() {
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .with_context(|| format!("making {}", dir.display()))?;
    }
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    let ends = (now + DAYS * DAY).div_euclid(DAY) * DAY;
    let key = KeyPair::generate().context("making the CA's key")?;
    let mut params = ca_params();
    valid(&mut params, now - DAY, ends)?;
    let cert = params.self_signed(&key).context("signing the CA")?;
    // The old expiry goes first: a half-written CA reads as none.
    let _ = std::fs::remove_file(dir.join("expires"));
    crate::host::conf::replace_private(&dir.join("ca.key"), &key.serialize_pem())?;
    crate::host::conf::replace_private(&dir.join("ca.pem"), &cert.pem())?;
    crate::host::conf::replace_private(&dir.join("expires"), &ends.to_string())?;
    Ok(true)
}

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// The roots the proxy checks an upstream against: the ones compiled into this binary, and
/// nothing a file, a variable or the system bundle says. The agent's own CA is in that bundle.
pub fn public_roots() -> Arc<rustls::RootCertStore> {
    static ROOTS: OnceLock<Arc<rustls::RootCertStore>> = OnceLock::new();
    let roots = ROOTS.get_or_init(|| {
        Arc::new(rustls::RootCertStore {
            roots: webpki_roots::TLS_SERVER_ROOTS.to_vec(),
        })
    });
    roots.clone()
}

/// The proxy's side of the connection to a bound host: `roots` only, HTTP/1.1 only.
pub fn client(roots: Arc<rustls::RootCertStore>) -> anyhow::Result<Arc<rustls::ClientConfig>> {
    let mut config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()?
        .with_root_certificates(roots)
        .with_no_client_auth();
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(Arc::new(config))
}

/// The CA as the daemon holds it: able to sign a leaf for a bound host. One leaf key per daemon
/// run, in memory. `Debug` prints nothing of it.
pub struct Authority {
    issuer: Issuer<'static, KeyPair>,
    leaf_key: KeyPair,
    /// By host: when the leaf ends, and what serves it.
    leaves: Mutex<HashMap<String, (i64, Arc<rustls::ServerConfig>)>>,
}

impl std::fmt::Debug for Authority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Authority")
    }
}

impl Authority {
    pub fn load(state: &Path) -> anyhow::Result<Authority> {
        // Never the parser's own message: it could quote the file.
        let text = std::fs::read_to_string(dir(state).join("ca.key"))
            .context("reading the CA's key (the container makes it when it starts)")?;
        let key =
            KeyPair::from_pem(&text).map_err(|_| anyhow::anyhow!("the CA's key is malformed"))?;
        Authority::new(key)
    }

    fn new(key: KeyPair) -> anyhow::Result<Authority> {
        Ok(Authority {
            issuer: Issuer::new(ca_params(), key),
            leaf_key: KeyPair::generate().context("making the leaf key")?,
            leaves: Mutex::new(HashMap::new()),
        })
    }

    /// What serves TLS as `host` (a name, no port): a certificate with that one name, from a
    /// day ago for 30 days, kept until a day before it ends. Nothing but a bound host is ever
    /// signed, so the map cannot outgrow the bindings.
    pub fn leaf(&self, host: &str, now: i64) -> anyhow::Result<Arc<rustls::ServerConfig>> {
        let mut leaves = self.leaves.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((ends, config)) = leaves.get(host) {
            if now < ends - DAY {
                return Ok(config.clone());
            }
        }
        let ends = (now + LEAF_DAYS * DAY).div_euclid(DAY) * DAY;
        let mut params = CertificateParams::new(vec![host.to_string()])
            .map_err(|_| anyhow::anyhow!("{host} is not a name a certificate can carry"))?;
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, host);
        valid(&mut params, now - DAY, ends)?;
        let cert = params
            .signed_by(&self.leaf_key, &self.issuer)
            .context("signing a leaf")?;
        let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.leaf_key.serialize_der()));
        let mut config = rustls::ServerConfig::builder_with_provider(provider())
            .with_safe_default_protocol_versions()?
            .with_no_client_auth()
            .with_single_cert(vec![cert.der().clone()], key)
            .context("a leaf's certificate")?;
        // HTTP/1.1 on both legs: one parser, one swap.
        config.alpn_protocols = vec![b"http/1.1".to_vec()];
        let config = Arc::new(config);
        leaves.insert(host.to_string(), (ends, config.clone()));
        Ok(config)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tokio_rustls::{TlsAcceptor, TlsConnector};

    const NOW: i64 = 1_800_000_000;

    /// A CA of the test's own: what signs with it, and the roots that trust it alone.
    pub(crate) fn test_ca(name: &str) -> (Authority, Arc<rustls::RootCertStore>) {
        let state = crate::test_dir(name);
        ensure(&state, crate::now()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        let pem = pem(&state).unwrap();
        let der = rustls::pki_types::pem::PemObject::from_pem_slice(pem.as_bytes()).unwrap();
        roots.add(der).unwrap();
        (Authority::load(&state).unwrap(), Arc::new(roots))
    }

    /// A handshake as `name` against `server`, with `roots`: the ALPN agreed, or the error.
    pub(crate) async fn handshake(
        server: Arc<rustls::ServerConfig>,
        roots: Arc<rustls::RootCertStore>,
        name: &str,
        alpn: &[&str],
    ) -> Result<Option<Vec<u8>>, String> {
        let (a, b) = tokio::io::duplex(16 * 1024);
        tokio::spawn(async move {
            let _ = TlsAcceptor::from(server).accept(b).await;
        });
        let mut config = (*client(roots).unwrap()).clone();
        config.alpn_protocols = alpn.iter().map(|p| p.as_bytes().to_vec()).collect();
        let name = rustls::pki_types::ServerName::try_from(name.to_string()).unwrap();
        let stream = TlsConnector::from(Arc::new(config)).connect(name, a).await;
        let stream = stream.map_err(|e| e.to_string())?;
        Ok(stream.get_ref().1.alpn_protocol().map(<[u8]>::to_vec))
    }

    #[test]
    fn ensure_makes_a_private_folder_and_key() {
        let state = crate::test_dir("ca-ensure");
        assert!(days_left(&state, NOW).is_err());
        assert!(ensure(&state, NOW).unwrap());
        let mode = |p: &str| {
            std::fs::metadata(dir(&state).join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
        };
        assert_eq!(mode(""), 0o700);
        assert_eq!(mode("ca.key"), 0o600);
        assert!(pem(&state)
            .unwrap()
            .starts_with("-----BEGIN CERTIFICATE-----"));
        let left = days_left(&state, NOW).unwrap();
        assert!((DAYS - 1..=DAYS).contains(&left), "{left}");
    }

    #[test]
    fn ensure_keeps_a_ca_with_200_days_left_and_replaces_one_with_100() {
        let state = crate::test_dir("ca-renew");
        ensure(&state, NOW).unwrap();
        let key = || std::fs::read(dir(&state).join("ca.key")).unwrap();
        let first = key();
        let later = NOW + (DAYS - 200) * DAY - DAY;
        assert!(days_left(&state, later).unwrap() >= 200);
        assert!(!ensure(&state, later).unwrap());
        assert_eq!(key(), first);
        let late = NOW + (DAYS - 100) * DAY;
        assert!(ensure(&state, late).unwrap());
        assert_ne!(key(), first, "the old key is gone");
        assert!(days_left(&state, late).unwrap() >= DAYS - 1);
        // A CA whose expiry cannot be read is no CA.
        std::fs::write(dir(&state).join("expires"), "soon").unwrap();
        assert!(ensure(&state, late).unwrap());
    }

    #[tokio::test]
    async fn a_leaf_verifies_for_its_host_and_no_other() {
        let (ca, roots) = test_ca("ca-leaf");
        let leaf = ca.leaf("api.x.test", crate::now()).unwrap();
        let ok = handshake(leaf.clone(), roots.clone(), "api.x.test", &["http/1.1"]).await;
        assert_eq!(ok.unwrap().as_deref(), Some(&b"http/1.1"[..]));
        for other in ["other.test", "x.test", "evil.api.x.test"] {
            let refused = handshake(leaf.clone(), roots.clone(), other, &["http/1.1"]).await;
            assert!(refused.is_err(), "{other}");
        }
        // Another CA's leaf for the same name is not this one's.
        let (planted, _) = test_ca("ca-leaf-planted");
        let theirs = planted.leaf("api.x.test", crate::now()).unwrap();
        assert!(handshake(theirs, roots, "api.x.test", &["http/1.1"])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn leaves_offer_http1_only() {
        let (ca, roots) = test_ca("ca-alpn");
        let leaf = ca.leaf("api.x.test", crate::now()).unwrap();
        let agreed = handshake(leaf, roots, "api.x.test", &["h2", "http/1.1"]).await;
        assert_eq!(agreed.unwrap().as_deref(), Some(&b"http/1.1"[..]));
    }

    #[test]
    fn the_same_leaf_is_served_until_a_day_before_it_expires() {
        let (ca, _) = test_ca("ca-leaf-cache");
        let first = ca.leaf("api.x.test", NOW).unwrap();
        assert!(Arc::ptr_eq(
            &first,
            &ca.leaf("api.x.test", NOW + 28 * DAY).unwrap()
        ));
        assert!(!Arc::ptr_eq(
            &first,
            &ca.leaf("api.x.test", NOW + 29 * DAY + 1).unwrap()
        ));
        assert!(!Arc::ptr_eq(&first, &ca.leaf("other.test", NOW).unwrap()));
        assert_eq!(format!("{ca:?}"), "Authority");
    }

    /// Review Focus 2: what the proxy trusts upstream is compiled in. A CA made in this
    /// container, the agent's or zephyr's own, is not among it, whatever the system bundle or
    /// `SSL_CERT_FILE` hold: nothing here reads either.
    #[tokio::test]
    async fn the_proxys_roots_are_the_compiled_in_ones_alone() {
        assert_eq!(
            public_roots().roots.len(),
            webpki_roots::TLS_SERVER_ROOTS.len()
        );
        let (planted, _) = test_ca("ca-planted");
        let leaf = planted.leaf("api.github.com", crate::now()).unwrap();
        let refused = handshake(leaf, public_roots(), "api.github.com", &["http/1.1"]).await;
        assert!(refused.is_err());
    }
}
