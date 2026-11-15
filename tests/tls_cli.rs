//! The real binary against a local HTTPS endpoint with a self-signed `CA:TRUE` certificate (what `openssl req -x509` makes).
mod common;
use common::*;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

/// Serve one fixed 401 over TLS; returns (url, certificate PEM).
fn https_server() -> (String, String) {
    let key = rcgen::KeyPair::generate().unwrap();
    let mut p = rcgen::CertificateParams::new(vec!["localhost".to_string()]).unwrap();
    p.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
    let cert = p.self_signed(&key).unwrap();
    let cfg = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
        .with_safe_default_protocol_versions().unwrap().with_no_client_auth()
        .with_single_cert(vec![cert.der().clone()], rustls::pki_types::PrivateKeyDer::try_from(key.serialize_der()).unwrap()).unwrap();
    let cfg = Arc::new(cfg);
    let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let cfg = cfg.clone();
            std::thread::spawn(move || {
                let Ok(conn) = rustls::ServerConnection::new(cfg) else { return };
                let mut tls = rustls::StreamOwned::new(conn, s);
                let mut buf = [0u8; 8192];
                if tls.read(&mut buf).unwrap_or(0) == 0 { return; }
                let body = r#"{"error":{"message":"no key"}}"#;
                let _ = tls.write_all(format!("HTTP/1.1 401 Unauthorized\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).as_bytes());
                tls.conn.send_close_notify();
                let _ = tls.flush();
            });
        }
    });
    (format!("https://localhost:{port}/v1"), cert.pem())
}

#[test]
fn a_certificate_pinned_through_ssl_cert_file_reaches_the_server() {
    let e = Env::new("tls_pin");
    let (url, pem) = https_server();
    let llm = FakeLlm { url, requests: Arc::new(Mutex::new(vec![])) };
    let f = e.dir.join("pinned.pem");
    std::fs::write(&f, &pem).unwrap();
    let o = e.cmd(&llm, &["run", "say ok"]).env("SSL_CERT_FILE", &f).output().unwrap();
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("LLM HTTP 401") && err(&o).contains("no key"), "{}", err(&o));
}

#[test]
fn without_the_pin_the_error_names_the_certificate_problem() {
    let e = Env::new("tls_nopin");
    let (url, _) = https_server();
    let llm = FakeLlm { url, requests: Arc::new(Mutex::new(vec![])) };
    let o = e.cmd(&llm, &["run", "say ok"]).env_remove("SSL_CERT_FILE").output().unwrap();
    assert_eq!(code(&o), 1);
    let s = err(&o);
    assert!(s.contains("error sending request for url") && s.contains("certificate not accepted: "), "{s}");
    assert!(!s.contains("sk-") && !s.contains("\"k\""), "no secrets: {s}");
}
