//! `zcode image pull` / `image gc` against an in-process fake registry. Own process: it points XDG_CACHE_HOME at a temp dir.
#![cfg(all(feature = "oci", target_os = "linux"))]
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use zcode_tools::container::registry::{blobs_dir, gc, pull};
use zcode_tools::container::{image_remove, images_dir};

fn hex(b: &[u8]) -> String { b.iter().map(|x| format!("{x:02x}")).collect() }
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::sync::Mutex;

fn dg(b: &[u8]) -> String { format!("sha256:{}", hex(&Sha256::digest(b))) }

fn entry(name: &str, flag: u8, data: &[u8], link: &str) -> Vec<u8> {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name.as_bytes());
    h[100..108].copy_from_slice(b"0000644\0");
    h[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
    h[156] = flag;
    h[157..157 + link.len()].copy_from_slice(link.as_bytes());
    h[257..263].copy_from_slice(b"ustar\0");
    let mut o = h.to_vec();
    o.extend_from_slice(data);
    o.resize(512 + data.len().div_ceil(512) * 512, 0);
    o
}
fn tar(entries: Vec<Vec<u8>>) -> Vec<u8> { let mut t: Vec<u8> = entries.concat(); t.extend(vec![0u8; 1024]); t }
fn gz(b: &[u8]) -> Vec<u8> {
    let mut e = flate2::write::GzEncoder::new(vec![], flate2::Compression::fast());
    e.write_all(b).unwrap();
    e.finish().unwrap()
}

struct Fake { port: u16, hits: Arc<Mutex<Vec<String>>> }
impl Fake {
    fn count(&self, what: &str) -> usize { self.hits.lock().unwrap().iter().filter(|h| h.as_str() == what).count() }
}

type Manifests = HashMap<String, (String, Vec<u8>)>;

/// A tiny registry: every /v2 request needs `Bearer tok` (else 401 + challenge), blobs sit behind a redirect.
fn serve(manifests: Manifests, blobs: HashMap<String, Vec<u8>>, corrupt: Option<String>) -> Fake {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    let hits: Arc<Mutex<Vec<String>>> = Default::default();
    let h2 = hits.clone();
    std::thread::spawn(move || {
        for s in l.incoming().flatten() {
            let (manifests, blobs, corrupt, hits) = (manifests.clone(), blobs.clone(), corrupt.clone(), h2.clone());
            std::thread::spawn(move || {
                let mut s = s;
                let mut buf = vec![];
                let mut b = [0u8; 1024];
                while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
                    let n = s.read(&mut b).unwrap_or(0);
                    if n == 0 { return; }
                    buf.extend_from_slice(&b[..n]);
                }
                let req = String::from_utf8_lossy(&buf).into_owned();
                let path = req.split_whitespace().nth(1).unwrap_or("").to_string();
                let authed = req.lines().any(|l| l.eq_ignore_ascii_case("authorization: bearer tok"));
                let send = |s: &mut std::net::TcpStream, head: &str, body: &[u8]| {
                    let _ = s.write_all(format!("HTTP/1.1 {head}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).as_bytes());
                    let _ = s.write_all(body);
                };
                if path.starts_with("/token") {
                    hits.lock().unwrap().push("token".into());
                    return send(&mut s, "200 OK\r\nContent-Type: application/json", br#"{"token":"tok"}"#);
                }
                if let Some(d) = path.strip_prefix("/cdn/") {
                    hits.lock().unwrap().push(format!("cdn:{d}"));
                    let mut body = blobs.get(d).cloned().unwrap_or_default();
                    if corrupt.as_deref() == Some(d) { body[0] ^= 0xff; }
                    return send(&mut s, "200 OK", &body);
                }
                if !authed {
                    let ch = format!("401 Unauthorized\r\nWWW-Authenticate: Bearer realm=\"http://127.0.0.1:{port}/token\",service=\"fake\",scope=\"repository:test/img:pull,push\"");
                    return send(&mut s, &ch, b"");
                }
                if let Some(r) = path.strip_prefix("/v2/test/img/manifests/") {
                    return match manifests.get(r) {
                        Some((ct, body)) => send(&mut s, &format!("200 OK\r\nContent-Type: {ct}"), body),
                        None => send(&mut s, "404 Not Found", b""),
                    };
                }
                if let Some(d) = path.strip_prefix("/v2/test/img/blobs/") {
                    return if blobs.contains_key(d) {
                        send(&mut s, &format!("307 Temporary Redirect\r\nLocation: /cdn/{d}"), b"")
                    } else {
                        send(&mut s, "404 Not Found", b"")
                    };
                }
                send(&mut s, "404 Not Found", b"");
            });
        }
    });
    Fake { port, hits }
}

fn manifest_for(layers: &[(&str, &[u8])]) -> (String, Vec<u8>) {
    let ls: Vec<Value> = layers.iter().map(|(ct, b)| json!({"mediaType": ct, "digest": dg(b), "size": b.len()})).collect();
    let m = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {"mediaType": "application/vnd.oci.image.config.v1+json", "digest": dg(b"{}"), "size": 2}, "layers": ls});
    let body = serde_json::to_vec(&m).unwrap();
    (dg(&body), body)
}

/// One scenario (it sets XDG_CACHE_HOME, which is process-wide): auth, index/platform, whiteouts, escapes, cache, corruption, gc.
#[tokio::test(flavor = "current_thread")]
async fn pull_end_to_end() {
    let tmp = std::env::temp_dir().join(format!("zc_oci_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp).unwrap();
    unsafe { std::env::set_var("XDG_CACHE_HOME", tmp.join("cache")); }
    let outside = tmp.join("outside");
    std::fs::create_dir_all(&outside).unwrap();

    let l1 = gz(&tar(vec![
        entry("bin/", b'5', b"", ""), entry("bin/hello", b'0', b"v1", ""),
        entry("etc/", b'5', b"", ""), entry("etc/old", b'0', b"x", ""),
        entry("data/", b'5', b"", ""), entry("data/a", b'0', b"a", ""), entry("data/b", b'0', b"b", ""),
        entry("gone", b'0', b"g", ""),
        entry("swap/", b'5', b"", ""), entry("swap/x", b'0', b"x", ""),
        entry("dev/", b'5', b"", ""), entry("dev/null", b'3', b"", ""),
        entry("../evil", b'0', b"evil", ""),
        entry("esc", b'2', b"", outside.to_str().unwrap()), entry("esc/pwned", b'0', b"pwned", ""),
    ]));
    // the second layer is a plain, uncompressed tar: the compression is detected from the bytes, not the media type
    let l2 = tar(vec![
        entry(".wh.gone", b'0', b"", ""), entry("etc/.wh.old", b'0', b"", ""),
        entry("data/", b'5', b"", ""), entry("data/c", b'0', b"c", ""), entry("data/.wh..wh..opq", b'0', b"", ""),
        entry("bin/hello", b'0', b"v2", ""),
        entry("swap", b'0', b"now a file", ""),
        entry("newdir/", b'5', b"", ""), entry("newdir/f", b'0', b"f", ""),
    ]);
    let (m_digest, m_body) = manifest_for(&[("application/vnd.oci.image.layer.v1.tar+gzip", &l1), ("application/vnd.oci.image.layer.v1.tar", &l2)]);
    let (other_digest, other_body) = manifest_for(&[("application/vnd.oci.image.layer.v1.tar", &l2)]);
    let index = json!({"schemaVersion": 2, "mediaType": "application/vnd.oci.image.index.v1+json", "manifests": [
        {"digest": other_digest, "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}},
        {"digest": "sha256:".to_string() + &"0".repeat(64), "platform": {"os": "unknown", "architecture": "unknown"}},
        {"digest": m_digest, "platform": {"os": "linux", "architecture": "amd64"}}]});
    let idx_body = serde_json::to_vec(&index).unwrap();
    let idx_digest = dg(&idx_body);
    let l3 = gz(&tar(vec![entry("f", b'0', b"bad layer", "")]));
    let (_, m3_body) = manifest_for(&[("application/vnd.oci.image.layer.v1.tar+gzip", &l3)]);
    let mut manifests: Manifests = HashMap::new();
    let idx_ct = "application/vnd.oci.image.index.v1+json".to_string();
    let img_ct = "application/vnd.oci.image.manifest.v1+json".to_string();
    manifests.insert("v1".into(), (idx_ct.clone(), idx_body.clone()));
    manifests.insert(idx_digest.clone(), (idx_ct, idx_body));
    manifests.insert("bad".into(), (img_ct.clone(), m3_body));
    manifests.insert(m_digest, (img_ct.clone(), m_body));
    manifests.insert(other_digest, (img_ct, other_body));
    let mut blobs = HashMap::new();
    for b in [&l1, &l2, &l3] { blobs.insert(dg(b), b.clone()); }
    let fake = serve(manifests, blobs, Some(dg(&l3)));
    let reference = format!("127.0.0.1:{}/test/img:v1", fake.port);

    // an index without the wanted platform lists what exists
    let e = pull(&reference, "x", Some("linux/riscv64")).await.unwrap_err();
    assert!(e.contains("no image for linux/riscv64") && e.contains("linux/amd64") && !e.contains("unknown"), "{e}");

    // a good pull: bearer auth (challenge -> token), index -> amd64 manifest, redirected blobs, layers applied in order
    let s = pull(&reference, "img", Some("linux/amd64")).await.unwrap();
    let root = images_dir().join("img/rootfs");
    let rd = |p: &str| std::fs::read_to_string(root.join(p)).unwrap();
    assert_eq!(rd("bin/hello"), "v2", "later layer wins");
    assert!(!root.join("gone").exists(), "whiteout removes a lower file");
    assert!(root.join("etc").is_dir() && !root.join("etc/old").exists(), "whiteout inside a dir");
    let mut data: Vec<_> = std::fs::read_dir(root.join("data")).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    data.sort();
    assert_eq!(data, ["c"], "opaque dir hides lower entries but keeps its own (even listed before the marker)");
    assert_eq!(rd("swap"), "now a file", "a file replaces a lower directory");
    assert_eq!(rd("newdir/f"), "f");
    assert!(!root.join(".wh.gone").exists() && !root.join("etc/.wh.old").exists() && !root.join("data/.wh..wh..opq").exists(), "whiteout markers are never written");
    assert!(!tmp.join("evil").exists() && !root.join("evil").exists() && !images_dir().join("evil").exists(), "path traversal is refused");
    assert!(!outside.join("pwned").exists(), "never written through a symlink");
    assert!(std::fs::symlink_metadata(root.join("esc")).unwrap().file_type().is_symlink());
    assert!(!root.join("dev/null").exists(), "device nodes are skipped");
    assert!(s.contains("warning") && s.contains("special file") && (s.contains("escapes") || s.contains("traverses")), "{s}");
    let meta: Value = serde_json::from_str(&std::fs::read_to_string(images_dir().join("img/image.json")).unwrap()).unwrap();
    assert_eq!(meta["layers"].as_array().unwrap().len(), 2);
    assert!(fake.count("token") >= 2, "one token per pull");
    assert_eq!(fake.count(&format!("cdn:{}", dg(&l1))), 1);

    // a repeat pull under another name is served from the blob cache, and a pinned digest is verified
    let pinned = format!("127.0.0.1:{}/test/img@{idx_digest}", fake.port);
    pull(&pinned, "img2", Some("linux/amd64")).await.unwrap();
    assert_eq!(fake.count(&format!("cdn:{}", dg(&l1))), 1, "cache hit: no second download");
    assert_eq!(std::fs::read_to_string(images_dir().join("img2/rootfs/bin/hello")).unwrap(), "v2");
    assert!(pull(&reference, "img", Some("linux/amd64")).await.unwrap_err().contains("already exists"));

    // a blob that does not hash to its digest is refused and leaves nothing behind
    let bad = format!("127.0.0.1:{}/test/img:bad", fake.port);
    let e = pull(&bad, "badimg", None).await.unwrap_err();
    assert!(e.contains("sha256 mismatch"), "{e}");
    assert!(!images_dir().join("badimg").exists());
    assert!(!blobs_dir().join(&dg(&l3)[7..]).exists(), "a corrupt blob never enters the cache");
    assert!(std::fs::read_dir(blobs_dir()).unwrap().flatten().all(|e| !e.file_name().to_string_lossy().contains(".part")), "no partial files");

    // gc keeps what installed images use and drops the rest
    std::fs::write(blobs_dir().join("f".repeat(64)), b"orphan").unwrap();
    std::fs::create_dir_all(images_dir().join(".dead.tmp999999999")).unwrap();
    let g = gc().unwrap();
    assert!(g.starts_with("removed 1 cached blob"), "{g}");
    assert!(blobs_dir().join(&dg(&l1)[7..]).exists() && !blobs_dir().join("f".repeat(64)).exists());
    assert!(!images_dir().join(".dead.tmp999999999").exists());
    image_remove("img").unwrap();
    assert!(gc().unwrap().starts_with("removed 0"), "img2 still uses both layers");
    image_remove("img2").unwrap();
    assert!(gc().unwrap().starts_with("removed 2 cached blob"));
    std::fs::remove_dir_all(&tmp).ok();
}
