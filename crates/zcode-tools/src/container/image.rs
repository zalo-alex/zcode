//! Local image store: `~/.cache/zcode/images/<name>/rootfs`. v1 imports a tar (plain, gzip, xz, zstd) or a
//! directory; the tar reader is in-process, refuses path traversal and never writes through a symlink.
use std::fs;
use std::io::{self, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Component, Path, PathBuf};

pub fn images_dir() -> PathBuf { crate::local::cache_dir().join("images") }

pub(super) fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && !n.starts_with('.') && n.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
}

pub fn rootfs_of(name: &str) -> Result<PathBuf, String> {
    if !valid_name(name) { return Err(format!("invalid image name `{name}`")); }
    let p = images_dir().join(name).join("rootfs");
    if p.is_dir() { Ok(p) } else {
        Err(format!("image `{name}` not found in {}; import one with `zcode image import <tar|dir> --name {name}`", images_dir().display()))
    }
}

pub fn list() -> Vec<(String, PathBuf)> {
    let mut v: Vec<(String, PathBuf)> = fs::read_dir(images_dir()).into_iter().flatten().flatten()
        .filter(|e| e.path().join("rootfs").is_dir() && valid_name(&e.file_name().to_string_lossy()))
        .map(|e| (e.file_name().to_string_lossy().into_owned(), e.path().join("rootfs"))).collect();
    v.sort();
    v
}

pub fn remove(name: &str) -> Result<(), String> {
    if !valid_name(name) { return Err(format!("invalid image name `{name}`")); }
    let p = images_dir().join(name);
    if !p.exists() { return Err(format!("no such image `{name}`")); }
    super::force_remove(&p);
    Ok(())
}

pub fn import(src: &Path, name: &str) -> Result<String, String> {
    if !valid_name(name) { return Err(format!("invalid image name `{name}` (letters, digits, . _ -)")); }
    let dest = images_dir().join(name);
    if dest.exists() { return Err(format!("image `{name}` already exists; remove it first with `zcode image rm {name}`")); }
    let tmp = images_dir().join(format!(".{name}.tmp{}", std::process::id()));
    super::force_remove(&tmp);
    let root = tmp.join("rootfs");
    fs::create_dir_all(&root).map_err(|e| e.to_string())?;
    let mut warnings = vec![];
    let r = if src.is_dir() { copy_tree(src, &root, &mut warnings) } else { untar(src, &root, &mut warnings) };
    if let Err(e) = r { super::force_remove(&tmp); return Err(e); }
    fs::rename(&tmp, &dest).map_err(|e| e.to_string())?;
    let mut s = format!("imported `{name}` into {}", dest.join("rootfs").display());
    for w in warnings.iter().take(10) { s.push_str(&format!("\nwarning: {w}")); }
    if warnings.len() > 10 { s.push_str(&format!("\nwarning: ... and {} more", warnings.len() - 10)); }
    Ok(s)
}

fn copy_tree(src: &Path, dst: &Path, warn: &mut Vec<String>) -> Result<(), String> {
    for e in fs::read_dir(src).map_err(|e| format!("{}: {e}", src.display()))?.flatten() {
        let (s, d) = (e.path(), dst.join(e.file_name()));
        let m = fs::symlink_metadata(&s).map_err(|e| e.to_string())?;
        let ft = m.file_type();
        if ft.is_symlink() {
            symlink(fs::read_link(&s).map_err(|e| e.to_string())?, &d).map_err(|e| format!("{}: {e}", d.display()))?;
        } else if ft.is_dir() {
            fs::create_dir(&d).map_err(|e| format!("{}: {e}", d.display()))?;
            copy_tree(&s, &d, warn)?;
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode((m.permissions().mode() & 0o1777) | 0o700));
        } else if ft.is_file() {
            fs::copy(&s, &d).map_err(|e| format!("{}: {e}", s.display()))?;
            let _ = fs::set_permissions(&d, fs::Permissions::from_mode(m.permissions().mode() & 0o1777));
        } else {
            warn.push(format!("skipped special file {}", s.display()));
        }
    }
    Ok(())
}

/// Join `rel` under `root` creating directories, refusing `..` and any symlinked parent.
fn safe_join(root: &Path, rel: &Path, make_parents: bool) -> Result<PathBuf, String> {
    let mut cur = root.to_path_buf();
    let comps: Vec<_> = rel.components().collect();
    for (i, c) in comps.iter().enumerate() {
        match c {
            Component::Normal(n) => {
                cur.push(n);
                if i + 1 == comps.len() { break; }
                match fs::symlink_metadata(&cur) {
                    Ok(m) if m.file_type().is_symlink() => return Err(format!("{} traverses a symlink", rel.display())),
                    Ok(m) if !m.is_dir() => return Err(format!("{}: parent is not a directory", rel.display())),
                    Ok(_) => {}
                    Err(_) if make_parents => { fs::create_dir(&cur).map_err(|e| e.to_string())?; let _ = fs::set_permissions(&cur, fs::Permissions::from_mode(0o755)); }
                    Err(e) => return Err(e.to_string()),
                }
            }
            Component::CurDir | Component::RootDir => {}
            _ => return Err(format!("{} escapes the image root", rel.display())),
        }
    }
    Ok(cur)
}

fn octal(b: &[u8]) -> u64 {
    if b.first().is_some_and(|x| x & 0x80 != 0) { // base-256
        return b.iter().skip(1).fold((b[0] & 0x7f) as u64, |a, x| (a << 8) | *x as u64);
    }
    let s: String = b.iter().take_while(|x| **x != 0 && **x != b' ').map(|x| *x as char).collect();
    u64::from_str_radix(s.trim(), 8).unwrap_or(0)
}

fn cstr(b: &[u8]) -> String { String::from_utf8_lossy(&b[..b.iter().position(|x| *x == 0).unwrap_or(b.len())]).into_owned() }

pub(super) fn open_stream(src: &Path) -> Result<Box<dyn Read>, String> {
    let mut f = fs::File::open(src).map_err(|e| format!("{}: {e}", src.display()))?;
    let mut magic = [0u8; 6];
    let n = f.read(&mut magic).unwrap_or(0);
    let prog = match &magic[..n] {
        [0x1f, 0x8b, ..] => Some(("gzip", "-dc")),
        [0xfd, b'7', b'z', b'X', b'Z', 0] => Some(("xz", "-dc")),
        [0x28, 0xb5, 0x2f, 0xfd, ..] => Some(("zstd", "-dc")),
        [b'B', b'Z', b'h', ..] => Some(("bzip2", "-dc")),
        _ => None,
    };
    match prog {
        None => Ok(Box::new(fs::File::open(src).map_err(|e| e.to_string())?)),
        Some((p, a)) => {
            let mut c = std::process::Command::new(p).arg(a).arg(src).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::null())
                .spawn().map_err(|e| format!("cannot run `{p}` to decompress the image: {e}"))?;
            Ok(Box::new(c.stdout.take().ok_or("no stdout")?))
        }
    }
}

fn untar(src: &Path, root: &Path, warn: &mut Vec<String>) -> Result<(), String> {
    untar_stream(open_stream(src)?, root, warn, false)
}

/// Delete whatever is at `p` without following a symlink (a directory goes with its contents).
fn remove_any(p: &Path) {
    match fs::symlink_metadata(p) {
        Ok(m) if m.is_dir() => { let _ = fs::remove_dir_all(p); }
        Ok(_) => { let _ = fs::remove_file(p); }
        Err(_) => {}
    }
}

/// Unpack one tar stream into `root`. With `whiteouts` (an OCI layer applied on top of earlier layers) `.wh.<name>` deletes
/// `<name>`, `.wh..wh..opq` empties its directory of what lower layers put there, and the markers themselves are never written.
pub(super) fn untar_stream(mut r: Box<dyn Read>, root: &Path, warn: &mut Vec<String>, whiteouts: bool) -> Result<(), String> {
    let mut created: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
    let (mut long_name, mut long_link): (Option<String>, Option<String>) = (None, None);
    let mut pax: Vec<(String, String)> = vec![];
    let mut hdr = [0u8; 512];
    let mut deferred_links: Vec<(PathBuf, PathBuf)> = vec![];
    loop {
        match r.read_exact(&mut hdr) { Ok(()) => {}, Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => break, Err(e) => return Err(e.to_string()) }
        if hdr.iter().all(|x| *x == 0) { break; }
        let size = octal(&hdr[124..136]);
        let flag = hdr[156];
        let mode = (octal(&hdr[100..108]) as u32) & 0o1777;
        let mut name = cstr(&hdr[0..100]);
        if &hdr[257..262] == b"ustar" { let p = cstr(&hdr[345..500]); if !p.is_empty() { name = format!("{p}/{name}"); } }
        let mut link = cstr(&hdr[157..257]);
        let padded = size.div_ceil(512) * 512;
        let read_data = |r: &mut Box<dyn Read>, n: u64| -> Result<Vec<u8>, String> {
            if n > 1 << 20 { return Err("oversized tar metadata".into()); }
            let mut b = vec![0u8; n as usize];
            r.read_exact(&mut b).map_err(|e| e.to_string())?;
            Ok(b)
        };
        let skip = |r: &mut Box<dyn Read>, n: u64| io::copy(&mut r.take(n), &mut io::sink()).map(|_| ()).map_err(|e| e.to_string());
        match flag {
            b'L' | b'K' | b'x' | b'g' => {
                let b = read_data(&mut r, size)?;
                skip(&mut r, padded - size)?;
                match flag {
                    b'L' => long_name = Some(cstr(&b)),
                    b'K' => long_link = Some(cstr(&b)),
                    b'x' => { // "<len> key=value\n" records
                        let s = String::from_utf8_lossy(&b).into_owned();
                        for rec in s.split('\n') { if let Some((_, kv)) = rec.split_once(' ') && let Some((k, v)) = kv.split_once('=') { pax.push((k.into(), v.into())); } }
                    }
                    _ => {}
                }
                continue;
            }
            _ => {}
        }
        if let Some(n) = long_name.take() { name = n; }
        if let Some(l) = long_link.take() { link = l; }
        for (k, v) in pax.drain(..) { if k == "path" { name = v; } else if k == "linkpath" { link = v; } }
        let rel = Path::new(name.trim_start_matches("./").trim_start_matches('/')).to_path_buf();
        if rel.as_os_str().is_empty() || rel == Path::new(".") { skip(&mut r, padded)?; continue; }
        if whiteouts && let Some(base) = rel.file_name().and_then(|n| n.to_str()).filter(|n| n.starts_with(".wh.")).map(String::from) {
            skip(&mut r, padded)?;
            let parent = rel.parent().unwrap_or(Path::new(""));
            let dir = match safe_join(root, parent, false) { Ok(d) => d, Err(e) => { warn.push(format!("skipped whiteout {name}: {e}")); continue; } };
            // the directory itself must be a real directory, never a symlink to somewhere else
            if !parent.as_os_str().is_empty() && !fs::symlink_metadata(&dir).is_ok_and(|m| m.is_dir()) { continue; }
            if base == ".wh..wh..opq" {
                for e in fs::read_dir(&dir).into_iter().flatten().flatten() { if !created.contains(&e.path()) { remove_any(&e.path()); } }
            } else {
                let victim = &base[4..];
                if victim.is_empty() || victim == "." || victim == ".." { warn.push(format!("skipped malformed whiteout {name}")); continue; }
                let victim = dir.join(victim);
                if !created.contains(&victim) { remove_any(&victim); }
            }
            continue;
        }
        let dest = match safe_join(root, &rel, true) { Ok(p) => p, Err(e) => { warn.push(format!("skipped {name}: {e}")); skip(&mut r, padded)?; continue; } };
        match flag {
            b'0' | 0 | b'7' => {
                remove_any(&dest);
                created.insert(dest.clone());
                let mut f = fs::File::create(&dest).map_err(|e| format!("{}: {e}", dest.display()))?;
                io::copy(&mut (&mut r).take(size), &mut f).map_err(|e| e.to_string())?;
                skip(&mut r, padded - size)?;
                let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode));
            }
            b'5' => {
                match fs::symlink_metadata(&dest) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => { remove_any(&dest); fs::create_dir(&dest).map_err(|e| format!("{}: {e}", dest.display()))?; }
                    Err(_) => fs::create_dir(&dest).map_err(|e| format!("{}: {e}", dest.display()))?,
                }
                created.insert(dest.clone());
                let _ = fs::set_permissions(&dest, fs::Permissions::from_mode(mode | 0o700));
                skip(&mut r, padded)?;
            }
            b'2' => {
                remove_any(&dest);
                created.insert(dest.clone());
                symlink(&link, &dest).map_err(|e| format!("{}: {e}", dest.display()))?;
                skip(&mut r, padded)?;
            }
            b'1' => { created.insert(dest.clone()); deferred_links.push((dest, PathBuf::from(link.trim_start_matches("./").trim_start_matches('/')))); skip(&mut r, padded)?; }
            _ => { warn.push(format!("skipped special file {name} (device nodes and fifos cannot be created rootless)")); skip(&mut r, padded)?; }
        }
    }
    for (dest, target) in deferred_links {
        match safe_join(root, &target, false) {
            Ok(t) => { let _ = fs::remove_file(&dest); if fs::hard_link(&t, &dest).is_err() && fs::copy(&t, &dest).is_err() { warn.push(format!("hardlink {} failed", dest.display())); } }
            Err(e) => warn.push(format!("skipped hardlink {}: {e}", dest.display())),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tar_entry(name: &str, flag: u8, data: &[u8], link: &str) -> Vec<u8> {
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

    #[test]
    fn a_directory_entry_over_a_symlink_never_changes_the_target() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("zc_untar_chmod_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        let outside = d.join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::set_permissions(&outside, fs::Permissions::from_mode(0o755)).unwrap();
        let mut t = vec![];
        t.extend(tar_entry("esc", b'2', b"", outside.to_str().unwrap()));
        t.extend(tar_entry("esc/", b'5', b"", ""));
        t.extend(vec![0u8; 1024]);
        fs::write(d.join("a.tar"), t).unwrap();
        let root = d.join("root");
        fs::create_dir_all(&root).unwrap();
        untar(&d.join("a.tar"), &root, &mut vec![]).unwrap();
        assert_eq!(fs::metadata(&outside).unwrap().permissions().mode() & 0o777, 0o755, "the symlink target was chmodded");
        assert!(fs::symlink_metadata(root.join("esc")).unwrap().is_dir(), "the symlink is replaced by the directory");
        fs::remove_dir_all(&d).ok();
    }

    #[test]
    fn extracts_and_blocks_traversal_and_symlink_escape() {
        let d = std::env::temp_dir().join(format!("zc_untar_{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        let mut t = vec![];
        t.extend(tar_entry("bin/", b'5', b"", ""));
        t.extend(tar_entry("bin/hello", b'0', b"hi", ""));
        t.extend(tar_entry("etc", b'2', b"", "/etc"));          // symlink pointing outside is fine as data...
        t.extend(tar_entry("etc/passwd", b'0', b"pwned", ""));   // ...but must never be written through
        t.extend(tar_entry("../escape", b'0', b"x", ""));
        t.extend(tar_entry("dev/null", b'3', b"", ""));
        t.extend(vec![0u8; 1024]);
        let tarf = d.join("a.tar");
        fs::write(&tarf, t).unwrap();
        let root = d.join("root");
        fs::create_dir_all(&root).unwrap();
        let mut w = vec![];
        untar(&tarf, &root, &mut w).unwrap();
        assert_eq!(fs::read_to_string(root.join("bin/hello")).unwrap(), "hi");
        assert!(fs::symlink_metadata(root.join("etc")).unwrap().file_type().is_symlink());
        assert!(!d.join("escape").exists() && !root.join("escape").exists());
        assert!(w.iter().any(|x| x.contains("escapes") || x.contains("traverses")), "{w:?}");
        assert!(w.iter().any(|x| x.contains("special file")), "{w:?}");
        assert_ne!(fs::read_to_string("/etc/passwd").unwrap_or_default(), "pwned");
        fs::remove_dir_all(&d).ok();
    }
}
