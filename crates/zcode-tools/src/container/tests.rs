//! Host-side robustness: replies from the guest are untrusted data.
use super::*;
use tokio::io::AsyncWriteExt;
use tokio::net::UnixStream;

async fn conn_with_guest() -> (Arc<Conn>, UnixStream) {
    let (host, guest) = UnixStream::pair().unwrap();
    (Conn::start(host), guest)
}

#[tokio::test]
async fn oversized_frame_from_the_guest_is_rejected() {
    let (c, mut g) = conn_with_guest().await;
    let call = tokio::spawn({ let c = c.clone(); async move { c.call("ping", json!({})).await } });
    let req = read_frame(&mut g).await.unwrap();
    assert_eq!(req["op"], "ping");
    // length prefix above the 8 MiB cap: the connection is dropped, the pending call fails
    g.write_all(&((crate::frames::MAX_FRAME as u32 + 1).to_be_bytes())).await.unwrap();
    assert!(call.await.unwrap().is_err());
    assert!(c.dead.load(Ordering::Relaxed));
    assert!(c.call("ping", json!({})).await.is_err(), "later calls fail fast");
}

#[tokio::test]
async fn malformed_and_unexpected_frames_are_rejected() {
    for bad in [&b"\0\0\0\x03xyz"[..], &b"\0\0\0\x02[]"[..], &b"\0\0\0\x0b{\"id\":1,\"x\":1}"[..], &b"\0\0\0\x15{\"id\":1,\"ev\":\"bogus\"}"[..]] {
        let (c, mut g) = conn_with_guest().await;
        let call = tokio::spawn({ let c = c.clone(); async move { c.call("ping", json!({})).await } });
        let _ = read_frame(&mut g).await.unwrap();
        g.write_all(bad).await.unwrap();
        assert!(call.await.unwrap().is_err(), "frame {bad:?} must drop the connection");
    }
}

#[tokio::test]
async fn well_formed_chunked_reply_is_assembled() {
    let (c, mut g) = conn_with_guest().await;
    let call = tokio::spawn({ let c = c.clone(); async move { c.call("read", json!({"path":"x"})).await } });
    let req = read_frame(&mut g).await.unwrap();
    let id = req["id"].as_u64().unwrap();
    crate::frames::write_frame(&mut g, &json!({"id": id, "ev": "out", "data": "ab"})).await.unwrap();
    crate::frames::write_frame(&mut g, &json!({"id": id, "ev": "out", "data": "cd"})).await.unwrap();
    // an event for an unknown id is ignored, not interpreted
    crate::frames::write_frame(&mut g, &json!({"id": id + 100, "ev": "out", "data": "zz"})).await.unwrap();
    crate::frames::write_frame(&mut g, &json!({"id": id, "ev": "done", "ok": true})).await.unwrap();
    let (out, done) = call.await.unwrap().unwrap();
    assert_eq!(out, "abcd");
    assert_eq!(done["ok"], true);
}

#[test]
fn sizes_parse() {
    assert_eq!(parse_size("2g"), Some(2 << 30));
    assert_eq!(parse_size("512m"), Some(512 << 20));
    assert_eq!(parse_size("1.5g"), Some((1.5 * (1u64 << 30) as f64) as u64));
    assert_eq!(parse_size("x"), None);
}
