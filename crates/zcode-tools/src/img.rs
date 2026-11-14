//! Image plumbing shared by the host and the tier-2 guest (no decoding here, so the guest stays small).
//!
//! A tool that produces an image returns an ordinary `String` of the form `\x01image:<mime>:<base64>` (the *image
//! result*), so `FileOps` and the frame protocol need no changes. The agent recognizes it, downscales it and turns it into
//! multi-part message content.

/// Source images above this many bytes are not attached (a text note is returned instead).
pub const MAX_SRC: usize = 5_000_000;
/// Marks a tool result as an image result.
pub const SENTINEL: &str = "\u{1}image:";

/// MIME type from magic bytes: JPEG, PNG, GIF, WebP.
pub fn sniff(b: &[u8]) -> Option<&'static str> {
    if b.starts_with(b"\xff\xd8\xff") { Some("image/jpeg") }
    else if b.starts_with(b"\x89PNG") { Some("image/png") }
    else if b.starts_with(b"GIF8") { Some("image/gif") }
    else if b.len() >= 12 && &b[..4] == b"RIFF" && &b[8..12] == b"WEBP" { Some("image/webp") }
    else { None }
}

pub fn too_big_note(bytes: usize) -> String {
    format!("[image not attached: {bytes} bytes is over the {} MB limit]", MAX_SRC / 1_000_000)
}

/// `\x01image:<mime>:<base64>`
pub fn encode_result(mime: &str, bytes: &[u8]) -> String {
    let mut s = String::with_capacity(SENTINEL.len() + mime.len() + 1 + bytes.len().div_ceil(3) * 4);
    s.push_str(SENTINEL);
    s.push_str(mime);
    s.push(':');
    s.push_str(&b64_encode(bytes));
    s
}

/// `(mime, base64)` of an image result.
pub fn parse_result(s: &str) -> Option<(&str, &str)> {
    s.strip_prefix(SENTINEL)?.split_once(':')
}

const ALPHA: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(b: &[u8]) -> String {
    let mut o = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        o.push(ALPHA[(n >> 18) as usize & 63] as char);
        o.push(ALPHA[(n >> 12) as usize & 63] as char);
        o.push(if c.len() > 1 { ALPHA[(n >> 6) as usize & 63] as char } else { '=' });
        o.push(if c.len() > 2 { ALPHA[n as usize & 63] as char } else { '=' });
    }
    o
}

fn val(c: u8) -> Option<u32> {
    match c { b'A'..=b'Z' => Some((c - b'A') as u32), b'a'..=b'z' => Some((c - b'a') as u32 + 26), b'0'..=b'9' => Some((c - b'0') as u32 + 52),
        b'+' => Some(62), b'/' => Some(63), _ => None }
}

/// Standard base64 (padding optional, ASCII whitespace ignored); `None` on any other character.
pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut o = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &c in s.as_bytes() {
        if c == b'=' { break; }
        if c.is_ascii_whitespace() { continue; }
        acc = acc << 6 | val(c)?;
        bits += 6;
        if bits >= 8 { bits -= 8; o.push((acc >> bits) as u8); acc &= (1 << bits) - 1; }
    }
    Some(o)
}

/// Whether text starts like a base64 (or `data:image/...;base64,`) image: decides if big command output is kept whole.
pub fn looks_like_image(head: &str) -> bool {
    let t = head.trim_start();
    let body = match t.strip_prefix("data:image/") { Some(r) => match r.split_once(";base64,") { Some((_, b)) => b, None => return false }, None => t };
    let h: String = body.chars().filter(|c| !c.is_ascii_whitespace()).take(16).collect();
    h.len() == 16 && b64_decode(&h).is_some_and(|b| sniff(&b).is_some())
}

/// Bash output that is nothing but base64 (or a `data:image/...;base64,` URL) of an image, as an image result.
/// Output over `MAX_SRC` once decoded gives the "too big" text note. Anything else (also base64 of non-images): `None`.
pub fn from_text_output(out: &str) -> Option<String> {
    let t = out.trim();
    if t.len() < 24 { return None; }
    let body = match t.strip_prefix("data:image/") {
        Some(rest) => rest.split_once(";base64,")?.1,
        None => t,
    };
    // cheap reject: the first bytes decide, then the whole text must be base64 alphabet + whitespace
    let head: String = body.chars().filter(|c| !c.is_ascii_whitespace()).take(16).collect();
    let mime = sniff(&b64_decode(&head)?)?;
    if !looks_like_image(t) { return None; }
    if !body.bytes().all(|c| c.is_ascii_whitespace() || c == b'=' || val(c).is_some()) { return None; }
    let clean: String = body.chars().filter(|c| !c.is_ascii_whitespace()).collect();
    let size = clean.len() / 4 * 3;
    if size > MAX_SRC { return Some(too_big_note(size)); }
    let mut s = String::with_capacity(SENTINEL.len() + mime.len() + 1 + clean.len());
    s.push_str(SENTINEL);
    s.push_str(mime);
    s.push(':');
    s.push_str(&clean);
    Some(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR";

    #[test]
    fn sniffs_magic_bytes() {
        assert_eq!(sniff(PNG), Some("image/png"));
        assert_eq!(sniff(b"\xff\xd8\xff\xe0abc"), Some("image/jpeg"));
        assert_eq!(sniff(b"GIF89a.."), Some("image/gif"));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WEBPVP8 "), Some("image/webp"));
        assert_eq!(sniff(b"RIFF\x10\0\0\0WAVEfmt "), None);
        assert_eq!(sniff(b"hello world"), None);
        assert_eq!(sniff(b""), None);
    }

    #[test]
    fn base64_roundtrip() {
        for n in 0..40usize {
            let v: Vec<u8> = (0..n as u8).map(|i| i.wrapping_mul(37)).collect();
            assert_eq!(b64_decode(&b64_encode(&v)).unwrap(), v);
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_decode("TW\nFu\n").unwrap(), b"Man");
        assert!(b64_decode("a$b").is_none());
    }

    #[test]
    fn bash_base64_becomes_an_image_result_untruncated() {
        let mut png = PNG.to_vec();
        png.extend(std::iter::repeat_n(7u8, 60_000));
        let b = b64_encode(&png);
        for out in [b.clone(), format!("{b}\n"), format!("data:image/png;base64,{b}\n")] {
            let r = from_text_output(&out).unwrap();
            let (mime, data) = parse_result(&r).unwrap();
            assert_eq!((mime, data.len()), ("image/png", b.len()));
            assert_eq!(b64_decode(data).unwrap(), png);
        }
        // wrapped like `base64` (76 columns)
        let wrapped = b.as_bytes().chunks(76).map(|c| std::str::from_utf8(c).unwrap()).collect::<Vec<_>>().join("\n");
        assert_eq!(parse_result(&from_text_output(&wrapped).unwrap()).unwrap().1, b);
    }

    #[test]
    fn non_images_stay_text() {
        assert!(from_text_output("hello world, this is plain output text").is_none());
        assert!(from_text_output(&b64_encode(b"just some base64 encoded text, not an image")).is_none());
        let b = b64_encode(PNG);
        assert!(from_text_output(&format!("{b}\n[exit 1]")).is_none());
    }

    #[test]
    fn oversize_gives_a_note() {
        let mut big = PNG.to_vec();
        big.resize(MAX_SRC + 10, 1);
        let r = from_text_output(&b64_encode(&big)).unwrap();
        assert!(r.starts_with("[image not attached") && parse_result(&r).is_none());
    }
}
