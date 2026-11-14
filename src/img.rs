//! Images for the model: downscale, turn tool results into multi-part content, caps.
//! The tools only produce an "image result" string (see `zcode_tools::img`); everything that needs a decoder lives here.
use crate::llm::{Content, Message, Part};
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, imageops::FilterType};
use std::io::Cursor;
use std::path::Path;
use zcode_tools::img::{MAX_SRC, b64_decode, b64_encode, parse_result, sniff, too_big_note};

/// Longest edge sent to the model (Anthropic rejects more).
pub const MAX_EDGE: u32 = 1568;
pub const JPEG_QUALITY: u8 = 80;
/// Images kept per tool result / user message, and per request (older ones are omitted).
pub const MAX_PER_RESULT: usize = 4;
pub const MAX_PER_REQUEST: usize = 8;
const MAX_PIXELS: u64 = 100_000_000;

pub const OMITTED: &str = "[image omitted: too many]";
pub const DROPPED: &str = "[image dropped]";
pub const BLIND: &str = "[image attached; this model cannot see it]";

/// An image ready for the wire.
#[derive(Debug)]
pub struct Img { pub url: String, pub w: u32, pub h: u32 }

fn format_of(mime: &str) -> Option<ImageFormat> {
    match mime { "image/jpeg" => Some(ImageFormat::Jpeg), "image/png" => Some(ImageFormat::Png), "image/gif" => Some(ImageFormat::Gif), "image/webp" => Some(ImageFormat::WebP), _ => None }
}

/// Pixel size from the header only.
fn dims(bytes: &[u8], mime: &str) -> Option<(u32, u32)> {
    let mut r = ImageReader::new(Cursor::new(bytes));
    r.set_format(format_of(mime)?);
    r.into_dimensions().ok()
}

/// `[image WxH]` for a data URL (`[image]` when it cannot be read).
pub fn label(url: &str) -> String {
    let d = url.strip_prefix("data:").and_then(|r| r.split_once(";base64,")).and_then(|(mime, b)| dims(&b64_decode(b)?, mime));
    match d { Some((w, h)) => format!("[image {w}x{h}]"), None => "[image]".into() }
}

/// Downscale (long edge `MAX_EDGE`), flatten alpha onto white and re-encode as JPEG q80. GIFs pass through untouched so
/// animation survives, and so do JPEGs that already fit. `Err` is a note to show instead of the image.
pub fn process(mime: &str, bytes: &[u8]) -> Result<Img, String> {
    if bytes.len() > MAX_SRC { return Err(too_big_note(bytes.len())); }
    let fmt = format_of(mime).ok_or("[image not attached: unsupported format]")?;
    let (w, h) = dims(bytes, mime).ok_or("[image not attached: could not be decoded]")?;
    if w == 0 || h == 0 || w as u64 * h as u64 > MAX_PIXELS { return Err("[image not attached: could not be decoded]".into()); }
    let url = |mime: &str, b: &[u8]| format!("data:{mime};base64,{}", b64_encode(b));
    if fmt == ImageFormat::Gif { return Ok(Img { url: url(mime, bytes), w, h }); }
    if fmt == ImageFormat::Jpeg && w.max(h) <= MAX_EDGE { return Ok(Img { url: url(mime, bytes), w, h }); }
    let mut r = ImageReader::new(Cursor::new(bytes));
    r.set_format(fmt);
    let decoded = (|| {
        let mut dec = r.into_decoder().ok()?;
        let orient = dec.orientation().ok();
        let mut img = DynamicImage::from_decoder(dec).ok()?;
        if let Some(o) = orient { img.apply_orientation(o); }
        Some(img)
    })().ok_or("[image not attached: could not be decoded]")?;
    let img = if decoded.width().max(decoded.height()) > MAX_EDGE { decoded.resize(MAX_EDGE, MAX_EDGE, FilterType::Lanczos3) } else { decoded };
    let (w, h) = (img.width(), img.height());
    let mut rgb = image::RgbImage::new(w, h);
    for (x, y, p) in img.to_rgba8().enumerate_pixels() {
        let a = p[3] as u32;
        let f = |c: u8| ((c as u32 * a + 255 * (255 - a) + 127) / 255) as u8;
        rgb.put_pixel(x, y, image::Rgb([f(p[0]), f(p[1]), f(p[2])]));
    }
    let mut out = Vec::new();
    rgb.write_with_encoder(image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, JPEG_QUALITY)).map_err(|_| "[image not attached: could not be encoded]")?;
    Ok(Img { url: url("image/jpeg", &out), w, h })
}

/// Replace images beyond `max` (counting from the first) with the omission note.
pub fn cap_parts(parts: Vec<Part>, max: usize) -> Vec<Part> {
    let mut n = 0;
    parts.into_iter().map(|p| if p.is_image() { n += 1; if n > max { Part::text(OMITTED) } else { p } } else { p }).collect()
}

/// At most `max` images in the whole conversation: the newest are kept, older ones become the omission note.
pub fn cap_request(msgs: &mut [Message], max: usize) {
    let total: usize = msgs.iter().map(|m| m.content.as_ref().map_or(0, |c| c.image_count())).sum();
    if total <= max { return; }
    let mut keep = max;
    for m in msgs.iter_mut().rev() {
        let Some(Content::Parts(ps)) = &mut m.content else { continue };
        for p in ps.iter_mut().rev() {
            if !p.is_image() { continue; }
            if keep > 0 { keep -= 1; } else { *p = Part::text(OMITTED); }
        }
    }
}

/// Old messages: images become `[image dropped]`. Returns the bytes saved.
pub fn drop_images(m: &mut Message) -> usize {
    let Some(Content::Parts(ps)) = &mut m.content else { return 0 };
    let mut saved = 0;
    for p in ps.iter_mut() {
        if let Part::ImageUrl { image_url } = p {
            saved += image_url.url.len().saturating_sub(DROPPED.len());
            *p = Part::text(DROPPED);
        }
    }
    saved
}

/// Turn a tool result into a message. An image result becomes `[text line, image]` (or text only when the model cannot
/// see, or the image cannot be used). Returns the text shown on text-only surfaces (events, the terminal) and the message.
pub fn tool_message(call_id: &str, what: Option<&str>, result: &str, vision: bool) -> (String, Message) {
    let Some((mime, b64)) = parse_result(result) else { return (result.to_string(), Message::tool(call_id, result)) };
    let name = what.map(|w| format!("Read image {w}")).unwrap_or_else(|| "Image from command output".into());
    let Some(bytes) = b64_decode(b64) else { let t = format!("{name}: [image not attached: invalid data]"); return (t.clone(), Message::tool(call_id, t)) };
    if !vision {
        let line = match dims(&bytes, mime) { Some((w, h)) => format!("{name} ({w}x{h})"), None => name };
        let t = format!("{line}\n{BLIND}");
        return (t.clone(), Message::tool(call_id, t));
    }
    match process(mime, &bytes) {
        Ok(i) => {
            let line = format!("{name} ({}x{})", i.w, i.h);
            (line.clone(), Message::tool_parts(call_id, cap_parts(vec![Part::text(line), Part::image(i.url)], MAX_PER_RESULT)))
        }
        Err(note) => { let t = format!("{name}: {note}"); (t.clone(), Message::tool(call_id, t)) }
    }
}

/// An image attached by the user: `(description, part)`; the part is `None` when the model cannot see.
#[derive(Debug)]
pub struct Attachment { pub line: String, pub part: Option<Part> }

/// Load an image file for `--image` / `/image`: same sniff, downscale and size cap as `read`.
pub fn load_attachment(path: &Path, vision: bool) -> Result<Attachment, String> {
    let meta = std::fs::metadata(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if !meta.is_file() { return Err(format!("{}: not a file", path.display())); }
    if meta.len() as usize > MAX_SRC { return Err(format!("{}: {}", path.display(), too_big_note(meta.len() as usize))); }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mime = sniff(&bytes).ok_or_else(|| format!("{}: not a supported image (jpeg, png, gif, webp)", path.display()))?;
    if !vision {
        let line = match dims(&bytes, mime) { Some((w, h)) => format!("{} ({w}x{h})", path.display()), None => path.display().to_string() };
        return Ok(Attachment { line, part: None });
    }
    let i = process(mime, &bytes).map_err(|n| format!("{}: {n}", path.display()))?;
    Ok(Attachment { line: format!("{} ({}x{})", path.display(), i.w, i.h), part: Some(Part::image(i.url)) })
}

/// The user message for `input` with attachments (plain text when there are none).
pub fn user_message(input: &str, atts: Vec<Attachment>) -> Message {
    if atts.is_empty() { return Message::text("user", input); }
    let mut parts = vec![Part::text(input)];
    for a in atts {
        match a.part {
            Some(p) => parts.push(p),
            None => parts.push(Part::text(format!("{BLIND} ({})", a.line))),
        }
    }
    let parts = cap_parts(parts, MAX_PER_RESULT);
    if parts.iter().all(|p| !p.is_image()) {
        // nothing to show the model: keep it a plain string
        return Message::text("user", parts.iter().map(|p| match p { Part::Text { text } => text.as_str(), _ => "" }).collect::<Vec<_>>().join("\n"));
    }
    Message { role: "user".into(), content: Some(Content::Parts(parts)), tool_calls: None, tool_call_id: None }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32, alpha: u8) -> Vec<u8> {
        let img = image::RgbaImage::from_fn(w, h, |x, y| image::Rgba([(x % 256) as u8, (y % 256) as u8, 128, alpha]));
        let mut out = Vec::new();
        img.write_with_encoder(image::codecs::png::PngEncoder::new(&mut Cursor::new(&mut out))).unwrap();
        out
    }
    fn data_of(i: &Img) -> Vec<u8> { b64_decode(i.url.split_once(";base64,").unwrap().1).unwrap() }

    #[test]
    fn sniffs_and_downscales_a_generated_png() {
        let bytes = png(3000, 1000, 255);
        assert_eq!(sniff(&bytes), Some("image/png"));
        let i = process("image/png", &bytes).unwrap();
        assert_eq!((i.w, i.h), (1568, 523));
        assert!(i.url.starts_with("data:image/jpeg;base64,"));
        let out = data_of(&i);
        assert_eq!(sniff(&out), Some("image/jpeg"));
        assert_eq!(dims(&out, "image/jpeg"), Some((1568, 523)));
        // small PNGs are not scaled up but still become JPEG
        let s = process("image/png", &png(40, 30, 255)).unwrap();
        assert_eq!((s.w, s.h), (40, 30));
        assert!(s.url.starts_with("data:image/jpeg"));
    }

    #[test]
    fn transparent_pixels_become_white() {
        let i = process("image/png", &png(8, 8, 0)).unwrap();
        let img = image::load_from_memory(&data_of(&i)).unwrap().to_rgb8();
        assert!(img.pixels().all(|p| p.0.iter().all(|&c| c >= 250)), "{:?}", img.get_pixel(0, 0));
    }

    #[test]
    fn jpeg_that_fits_and_gif_pass_through() {
        let mut jpg = Vec::new();
        image::RgbImage::new(20, 10).write_with_encoder(image::codecs::jpeg::JpegEncoder::new(&mut jpg)).unwrap();
        let i = process("image/jpeg", &jpg).unwrap();
        assert_eq!(data_of(&i), jpg);
        let mut gif = Vec::new();
        image::codecs::gif::GifEncoder::new(&mut gif).encode(&[0u8; 4 * 4 * 2], 4, 2, image::ExtendedColorType::Rgba8).unwrap();
        let g = process("image/gif", &gif).unwrap();
        assert_eq!((data_of(&g), g.w, g.h), (gif, 4, 2));
    }

    #[test]
    fn undecodable_and_oversize_give_notes() {
        let mut bad = b"\x89PNG\r\n\x1a\n".to_vec();
        bad.extend([1u8; 30]);
        assert!(process("image/png", &bad).unwrap_err().contains("could not be decoded"));
        assert!(process("image/png", &vec![0u8; MAX_SRC + 1]).unwrap_err().contains("over the 5 MB limit"));
    }

    #[test]
    fn tool_result_becomes_parts_or_text() {
        let r = zcode_tools::img::encode_result("image/png", &png(1440, 900, 255));
        let (shown, m) = tool_message("c1", Some("/w/shot.png"), &r, true);
        assert_eq!(shown, "Read image /w/shot.png (1440x900)");
        let Some(Content::Parts(ps)) = &m.content else { panic!("not parts") };
        assert!(matches!(&ps[0], Part::Text { text } if text == "Read image /w/shot.png (1440x900)") && ps[1].is_image() && ps.len() == 2);
        assert_eq!(m.tool_call_id.as_deref(), Some("c1"));
        // text-only surfaces
        assert_eq!(m.content.as_ref().unwrap().to_text(), "Read image /w/shot.png (1440x900)\n[image 1440x900]");
        // a model that cannot see gets one string
        let (shown, m) = tool_message("c1", Some("/w/shot.png"), &r, false);
        assert_eq!(shown, "Read image /w/shot.png (1440x900)\n[image attached; this model cannot see it]");
        assert_eq!(m.content.as_ref().unwrap().as_text(), Some(shown.as_str()));
        // plain results are untouched
        let (shown, m) = tool_message("c2", None, "hello", true);
        assert_eq!((shown.as_str(), m.content.as_ref().unwrap().as_text()), ("hello", Some("hello")));
    }

    #[test]
    fn caps_per_result_and_per_request() {
        let ps = vec![Part::image("a"); 6];
        let c = cap_parts(ps, 4);
        assert_eq!(c.iter().filter(|p| p.is_image()).count(), 4);
        assert!(matches!(&c[5], Part::Text { text } if text == OMITTED));
        let mut msgs: Vec<Message> = (0..5).map(|i| Message::tool_parts(&format!("c{i}"), vec![Part::text("t"), Part::image("x"), Part::image("y")])).collect();
        cap_request(&mut msgs, 8);
        let n: Vec<usize> = msgs.iter().map(|m| m.content.as_ref().unwrap().image_count()).collect();
        assert_eq!(n, vec![0, 2, 2, 2, 2]);
    }

    #[test]
    fn dropping_images_keeps_the_text() {
        let mut m = Message::tool_parts("c", vec![Part::text("Read image a (1x1)"), Part::image("data:image/png;base64,AAAA")]);
        assert!(drop_images(&mut m) > 0);
        assert_eq!(m.content.unwrap().to_text(), format!("Read image a (1x1)\n{DROPPED}"));
    }

    #[test]
    fn user_attachments() {
        let dir = std::env::temp_dir().join(format!("zcode_img_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("a.png");
        std::fs::write(&p, png(30, 20, 255)).unwrap();
        let a = load_attachment(&p, true).unwrap();
        assert!(a.line.ends_with("a.png (30x20)") && a.part.is_some());
        let m = user_message("what is this?", vec![a]);
        assert!(matches!(&m.content, Some(Content::Parts(ps)) if ps.len() == 2));
        let a = load_attachment(&p, false).unwrap();
        let m = user_message("what is this?", vec![a]);
        let c = m.content.as_ref().unwrap();
        assert!(c.as_text().is_some_and(|s| s.starts_with("what is this?") && s.contains(BLIND)));
        let t = dir.join("t.txt");
        std::fs::write(&t, "hello").unwrap();
        assert!(load_attachment(&t, true).unwrap_err().contains("not a supported image"));
        assert_eq!(user_message("hi", vec![]).content.unwrap().as_text(), Some("hi"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
