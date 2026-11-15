//! Certificates pinned through `SSL_CERT_FILE`, against a real local HTTPS server. The certificate file is passed to
//! `tls::client_builder_with`, never through the process environment, so the tests do not race.
use rcgen::{BasicConstraints, CertificateParams, ExtendedKeyUsagePurpose, IsCa, KeyPair, date_time_ymd};
use std::path::PathBuf;
use std::sync::Arc;
use zcode_tools::tls;

struct Cert { pem: String, der: rustls::pki_types::CertificateDer<'static>, key: KeyPair }

fn params(names: &[&str], ca: bool) -> CertificateParams {
    let mut p = CertificateParams::new(names.iter().map(|s| s.to_string()).collect::<Vec<_>>()).unwrap();
    p.is_ca = if ca { IsCa::Ca(BasicConstraints::Unconstrained) } else { IsCa::ExplicitNoCa };
    p
}

/// The usual `openssl req -x509` certificate: self-signed, `CA:TRUE`, used as the server's own certificate.
fn self_signed(names: &[&str], tweak: impl FnOnce(&mut CertificateParams)) -> Cert {
    let key = KeyPair::generate().unwrap();
    let mut p = params(names, true);
    tweak(&mut p);
    let c = p.self_signed(&key).unwrap();
    Cert { pem: c.pem(), der: c.der().clone(), key }
}

fn serve(cert: Vec<rustls::pki_types::CertificateDer<'static>>, key: &KeyPair) -> u16 {
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
        .with_single_cert(cert, rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap()).unwrap();
    let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(cfg));
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    l.set_nonblocking(true).unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let l = tokio::net::TcpListener::from_std(l).unwrap();
            loop {
                let Ok((s, _)) = l.accept().await else { continue };
                let a = acceptor.clone();
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let Ok(mut s) = a.accept(s).await else { return };
                    let mut buf = [0u8; 4096];
                    let _ = s.read(&mut buf).await;
                    let body = r#"{"error":{"message":"no key"}}"#;
                    let _ = s.write_all(format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes()).await;
                    let _ = s.shutdown().await;
                });
            }
        });
    });
    port
}

fn file(name: &str, pem: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("zc_tls_{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    let p = d.join(name);
    std::fs::write(&p, pem).unwrap();
    p
}

/// `Ok(status)` when the request reached the server, `Err(explained error)` otherwise.
async fn get(url: &str, cert_file: Option<&PathBuf>) -> Result<u16, String> {
    let c = tls::client_builder_with(cert_file.map(|p| p.as_path())).build().unwrap();
    match c.get(url).send().await { Ok(r) => Ok(r.status().as_u16()), Err(e) => Err(tls::explain(&e)) }
}

/// A system-like bundle (a few unrelated certificates) with `extra` at the end, the way `SSL_CERT_FILE` usually looks.
fn bundle(extra: &str) -> String {
    let others: String = (0..3).map(|i| self_signed(&[&format!("other{i}.example")], |_| {}).pem).collect();
    format!("{others}{extra}")
}

#[tokio::test]
async fn a_pinned_self_signed_ca_certificate_is_accepted() {
    let c = self_signed(&["localhost"], |_| {});
    let port = serve(vec![c.der.clone()], &c.key);
    let f = file("pinned.pem", &bundle(&c.pem));
    assert_eq!(get(&format!("https://localhost:{port}/"), Some(&f)).await, Ok(401));
}

#[tokio::test]
async fn the_same_certificate_not_pinned_fails_with_a_readable_error() {
    let c = self_signed(&["localhost"], |_| {});
    let port = serve(vec![c.der.clone()], &c.key);
    let f = file("unpinned.pem", &bundle(""));
    for file in [Some(&f), None] {
        let e = get(&format!("https://localhost:{port}/"), file).await.unwrap_err();
        assert!(e.contains(&format!("https://localhost:{port}/")) && e.contains("certificate not accepted: "), "{e}");
    }
}

#[tokio::test]
async fn a_pinned_certificate_still_has_to_match_the_host_name() {
    let c = self_signed(&["localhost"], |_| {});
    let port = serve(vec![c.der.clone()], &c.key);
    let f = file("name.pem", &c.pem);
    // 127.0.0.1 is not in the SAN (only DNS:localhost)
    let e = get(&format!("https://127.0.0.1:{port}/"), Some(&f)).await.unwrap_err();
    assert!(e.contains("certificate not accepted: ") && e.contains("not valid for name"), "{e}");
}

#[tokio::test]
async fn a_pinned_certificate_that_is_expired_or_not_yet_valid_is_refused() {
    let old = self_signed(&["localhost"], |p| { p.not_before = date_time_ymd(2001, 1, 1); p.not_after = date_time_ymd(2002, 1, 1); });
    let port = serve(vec![old.der.clone()], &old.key);
    let e = get(&format!("https://localhost:{port}/"), Some(&file("old.pem", &old.pem))).await.unwrap_err();
    assert!(e.contains("Expired"), "{e}");
    let future = self_signed(&["localhost"], |p| { p.not_before = date_time_ymd(2090, 1, 1); p.not_after = date_time_ymd(2091, 1, 1); });
    let port = serve(vec![future.der.clone()], &future.key);
    let e = get(&format!("https://localhost:{port}/"), Some(&file("future.pem", &future.pem))).await.unwrap_err();
    assert!(e.contains("NotValidYet"), "{e}");
}

#[tokio::test]
async fn a_different_certificate_than_the_pinned_one_is_refused() {
    let served = self_signed(&["localhost"], |_| {});
    let pinned = self_signed(&["localhost"], |_| {}); // same name, same shape, other key
    let port = serve(vec![served.der.clone()], &served.key);
    let e = get(&format!("https://localhost:{port}/"), Some(&file("other.pem", &bundle(&pinned.pem)))).await.unwrap_err();
    assert!(e.contains("certificate not accepted: "), "{e}");
}

#[tokio::test]
async fn a_leaf_signed_by_a_pinned_ca_goes_through_the_normal_verifier() {
    let ca_key = KeyPair::generate().unwrap();
    let ca_params = params(&["Test CA"], true);
    let ca = ca_params.self_signed(&ca_key).unwrap();
    let issuer = rcgen::Issuer::from_params(&ca_params, &ca_key);
    let leaf_key = KeyPair::generate().unwrap();
    let mut lp = params(&["localhost"], false);
    lp.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
    let leaf = lp.signed_by(&leaf_key, &issuer).unwrap();
    let port = serve(vec![leaf.der().clone()], &leaf_key);
    assert_eq!(get(&format!("https://localhost:{port}/"), Some(&file("ca.pem", &bundle(&ca.pem())))).await, Ok(401));
    // without the CA in the file the same server is refused
    let e = get(&format!("https://localhost:{port}/"), Some(&file("noca.pem", &bundle(""))))
        .await.unwrap_err();
    assert!(e.contains("certificate not accepted: "), "{e}");
}

#[tokio::test]
async fn refused_connections_show_their_root_cause_too() {
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    drop(l);
    let e = get(&format!("https://127.0.0.1:{port}/"), None).await.unwrap_err();
    assert!(e.starts_with("error sending request for url") && e.to_lowercase().contains("refused"), "{e}");
}
