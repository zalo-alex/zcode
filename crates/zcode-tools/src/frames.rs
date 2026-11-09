//! Host <-> guest wire format: 4-byte big-endian length + JSON, at most `MAX_FRAME` bytes.
//! Requests: `{"id":N,"op":"exec|bash_output|bash_kill|read|write|edit|multiedit|glob|grep|ls|ping|shutdown","args":{..}}`.
//! Replies for one id: any number of `{"id":N,"ev":"out","data":".."}` chunks, then one
//! `{"id":N,"ev":"done","ok":bool, "out"?:"..","err"?:"..","code"?:N,"timed_out"?:bool}`.
use serde_json::Value;
use std::io;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_FRAME: usize = 8 * 1024 * 1024;
/// payloads above this are split into several `out` events
pub const CHUNK: usize = 1024 * 1024;

fn bad(m: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, m.to_string()) }

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> io::Result<Value> {
    let mut len = [0u8; 4];
    r.read_exact(&mut len).await?;
    let n = u32::from_be_bytes(len) as usize;
    if n == 0 || n > MAX_FRAME { return Err(bad("frame length out of bounds")); }
    let mut buf = vec![0u8; n];
    r.read_exact(&mut buf).await?;
    let v: Value = serde_json::from_slice(&buf).map_err(|_| bad("malformed JSON frame"))?;
    if !v.is_object() { return Err(bad("frame is not an object")); }
    Ok(v)
}

pub async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, v: &Value) -> io::Result<()> {
    let b = serde_json::to_vec(v).map_err(|e| bad(&e.to_string()))?;
    if b.len() > MAX_FRAME { return Err(bad("frame too large")); }
    w.write_all(&(b.len() as u32).to_be_bytes()).await?;
    w.write_all(&b).await?;
    w.flush().await
}

/// Split `s` into chunks of at most `CHUNK` bytes on char boundaries.
pub fn chunks(s: &str) -> Vec<&str> {
    let mut v = vec![];
    let mut i = 0;
    while i < s.len() {
        let mut j = (i + CHUNK).min(s.len());
        while !s.is_char_boundary(j) { j -= 1; }
        v.push(&s[i..j]);
        i = j;
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn roundtrip_and_bounds() {
        let (mut a, mut b) = tokio::io::duplex(64 * 1024);
        write_frame(&mut a, &json!({"id":1,"op":"ping"})).await.unwrap();
        assert_eq!(read_frame(&mut b).await.unwrap()["op"], "ping");
        // oversized length prefix is rejected without allocating
        a.write_all(&((MAX_FRAME as u32 + 1).to_be_bytes())).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
        // malformed JSON and non-objects are rejected
        let (mut a, mut b) = tokio::io::duplex(1024);
        a.write_all(&[0, 0, 0, 3, b'x', b'y', b'z']).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
        a.write_all(&[0, 0, 0, 2, b'[', b']']).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
        // zero-length
        a.write_all(&[0, 0, 0, 0]).await.unwrap();
        assert!(read_frame(&mut b).await.is_err());
        // too large to send
        let big = json!({"x": "a".repeat(MAX_FRAME)});
        let (mut a, _b) = tokio::io::duplex(1024);
        assert!(write_frame(&mut a, &big).await.is_err());
    }

    #[test]
    fn chunking_keeps_utf8() {
        let s = "é".repeat(CHUNK);
        let c = chunks(&s);
        assert!(c.len() >= 2 && c.iter().all(|x| x.len() <= CHUNK) && c.concat() == s);
    }
}
