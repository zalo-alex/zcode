//! TLS for every outgoing HTTPS call: the `ring` provider, certificates pinned through `SSL_CERT_FILE`, readable errors.
//!
//! Everything validates exactly as `reqwest` does by default (the platform verifier: the system roots plus `SSL_CERT_FILE`),
//! with one addition. webpki refuses a certificate that says `CA:TRUE` when it is the *server's* certificate
//! (`CaUsedAsEndEntity`), which is what a self-signed certificate made with `openssl req -x509` usually is, even though
//! the user put that very file in `SSL_CERT_FILE`. So when the server's end-entity certificate is byte-for-byte one of the
//! certificates in `SSL_CERT_FILE`, it is accepted, still subject to its validity dates and to the host name matching its
//! SAN. Any other certificate goes to the normal verifier.
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::verify_server_name;
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime, pem::PemObject};
use rustls::server::ParsedCertificate;
use rustls::{CertificateError, ClientConfig, DigitallySignedStruct, Error, SignatureScheme};
use std::collections::HashSet;
use std::path::Path;
use std::sync::Arc;

/// Install the `ring` crypto provider for rustls (once). Call before building the first `reqwest::Client`.
pub fn init_tls() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| { let _ = rustls::crypto::ring::default_provider().install_default(); });
}

/// A client builder with zcode's TLS setup; reads `SSL_CERT_FILE`.
pub fn client_builder() -> reqwest::ClientBuilder {
    client_builder_with(std::env::var_os("SSL_CERT_FILE").as_deref().map(Path::new))
}

/// Like `client_builder`, with the certificate file given instead of read from the environment.
/// Without pinned certificates the builder is reqwest's default one, untouched.
pub fn client_builder_with(cert_file: Option<&Path>) -> reqwest::ClientBuilder {
    init_tls();
    let b = reqwest::Client::builder();
    let Some(path) = cert_file else { return b };
    let pinned = load_pem(path);
    if pinned.is_empty() { return b; }
    match pinned_config(pinned) {
        Ok(cfg) => b.tls_backend_preconfigured(cfg),
        // the verifier could not be built (no platform roots at all): keep reqwest's own behavior and error
        Err(_) => b,
    }
}

/// The certificates of a PEM file (a bundle, usually): unreadable or malformed input gives none.
fn load_pem(path: &Path) -> Vec<CertificateDer<'static>> {
    let Ok(pem) = std::fs::read(path) else { return vec![] };
    CertificateDer::pem_slice_iter(&pem).flatten().collect()
}

fn pinned_config(pinned: Vec<CertificateDer<'static>>) -> Result<ClientConfig, Error> {
    let provider = CryptoProvider::get_default().cloned().unwrap_or_else(|| Arc::new(rustls::crypto::ring::default_provider()));
    // the pinned certificates are also roots for the normal verifier, so a CA pinned here validates the leaves it signed
    #[cfg(unix)]
    let inner = rustls_platform_verifier::Verifier::new_with_extra_roots(pinned.clone(), provider.clone());
    #[cfg(not(unix))]
    let inner = rustls_platform_verifier::Verifier::new(provider.clone());
    let inner = inner?;
    let verifier = Pinned { inner: Arc::new(inner), pinned: pinned.iter().map(|c| c.as_ref().to_vec()).collect(), algs: provider.signature_verification_algorithms };
    let mut cfg = ClientConfig::builder_with_provider(provider).with_safe_default_protocol_versions()?
        .dangerous().with_custom_certificate_verifier(Arc::new(verifier)).with_no_client_auth();
    cfg.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(cfg)
}

#[derive(Debug)]
struct Pinned {
    inner: Arc<dyn ServerCertVerifier>,
    pinned: HashSet<Vec<u8>>,
    algs: WebPkiSupportedAlgorithms,
}

impl ServerCertVerifier for Pinned {
    fn verify_server_cert(&self, end_entity: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], server_name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, Error> {
        if !self.pinned.contains(end_entity.as_ref()) {
            return self.inner.verify_server_cert(end_entity, intermediates, server_name, ocsp, now);
        }
        // trusted by identity; dates and name are still checked
        let (from, to) = validity(end_entity).ok_or(Error::InvalidCertificate(CertificateError::BadEncoding))?;
        let now = now.as_secs() as i64;
        if now < from { return Err(Error::InvalidCertificate(CertificateError::NotValidYet)); }
        if now > to { return Err(Error::InvalidCertificate(CertificateError::Expired)); }
        verify_server_name(&ParsedCertificate::try_from(end_entity)?, server_name)?;
        Ok(ServerCertVerified::assertion())
    }
    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        verify_tls12_signature(message, cert, dss, &self.algs)
    }
    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, Error> {
        verify_tls13_signature(message, cert, dss, &self.algs)
    }
    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> { self.algs.supported_schemes() }
}

/// One DER element: `(tag, content, rest)`.
fn tlv(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, b) = b.split_first()?;
    let (&l, b) = b.split_first()?;
    let (len, b) = if l < 0x80 { (l as usize, b) } else {
        let n = (l & 0x7f) as usize;
        if n == 0 || n > 4 || b.len() < n { return None; }
        (b[..n].iter().fold(0usize, |a, &x| a << 8 | x as usize), &b[n..])
    };
    if b.len() < len { return None; }
    Some((tag, &b[..len], &b[len..]))
}

/// `(notBefore, notAfter)` of a certificate, in unix seconds.
fn validity(cert: &[u8]) -> Option<(i64, i64)> {
    let (_, cert, _) = tlv(cert)?;
    let (_, tbs, _) = tlv(cert)?;
    let mut r = tbs;
    if r.first() == Some(&0xa0) { r = tlv(r)?.2; } // [0] version
    for _ in 0..3 { r = tlv(r)?.2; } // serial, signature algorithm, issuer
    let (_, v, _) = tlv(r)?;
    let (t1, a, rest) = tlv(v)?;
    let (t2, b, _) = tlv(rest)?;
    Some((der_time(t1, a)?, der_time(t2, b)?))
}

/// UTCTime (`YYMMDDHHMMSSZ`) or GeneralizedTime (`YYYYMMDDHHMMSSZ`) as unix seconds.
fn der_time(tag: u8, b: &[u8]) -> Option<i64> {
    let s = std::str::from_utf8(b).ok()?.strip_suffix('Z')?;
    let (year, rest) = match tag {
        0x17 if s.len() == 12 => { let yy: i64 = s[..2].parse().ok()?; (if yy >= 50 { 1900 + yy } else { 2000 + yy }, &s[2..]) }
        0x18 if s.len() == 14 => (s[..4].parse().ok()?, &s[4..]),
        _ => return None,
    };
    let f = |i: usize| rest.get(i..i + 2)?.parse::<i64>().ok();
    let (mon, day, h, mi, sec) = (f(0)?, f(2)?, f(4)?, f(6)?, f(8)?);
    // days from civil (Howard Hinnant)
    let y = if mon <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if mon > 2 { mon - 3 } else { mon + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((era * 146097 + doe - 719468) * 86400 + h * 3600 + mi * 60 + sec)
}

/// One line for a failed request: reqwest's own message plus the root cause, with a certificate failure spelled out
/// (`error sending request for url (...): certificate not accepted: UnknownIssuer`). Nothing but what the error chain says:
/// never the key or the body.
pub fn explain(e: &reqwest::Error) -> String {
    use std::error::Error as _;
    let msg = e.to_string();
    let mut root = None;
    let mut c = e.source();
    while let Some(x) = c { root = Some(x.to_string()); c = x.source(); }
    match root {
        Some(r) if r != msg => format!("{msg}: {}", tidy(&r)),
        _ => msg,
    }
}

/// The request failed because the server's certificate was rejected (retrying cannot help).
pub fn is_certificate_error(e: &reqwest::Error) -> bool {
    use std::error::Error as _;
    let mut c = e.source();
    while let Some(x) = c {
        if x.to_string().starts_with("invalid peer certificate: ") { return true; }
        c = x.source();
    }
    false
}

fn tidy(cause: &str) -> String {
    match cause.strip_prefix("invalid peer certificate: ") {
        Some(c) => format!("certificate not accepted: {}", c.strip_prefix("Other(OtherError(").and_then(|x| x.strip_suffix("))")).unwrap_or(c)),
        None => cause.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn der_times_and_tidy_messages() {
        assert_eq!(der_time(0x17, b"700101000000Z"), Some(0));
        assert_eq!(der_time(0x17, b"491231235959Z"), Some(2524607999));
        assert_eq!(der_time(0x18, b"20000229120000Z"), Some(951825600));
        assert_eq!(der_time(0x17, b"junk"), None);
        assert_eq!(tidy("invalid peer certificate: Other(OtherError(CaUsedAsEndEntity))"), "certificate not accepted: CaUsedAsEndEntity");
        assert_eq!(tidy("invalid peer certificate: UnknownIssuer"), "certificate not accepted: UnknownIssuer");
        assert_eq!(tidy("Connection refused (os error 111)"), "Connection refused (os error 111)");
    }
}
