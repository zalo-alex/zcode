//! File tools over the local filesystem (tier 0/1; the guest binary reuses this for tier 2).
//! With a `PathGuard` every open is confined by the sandbox policy. Stale-edit detection: each read
//! records a content hash per file; write/edit/multiedit fail if the file changed since.
use crate::edit_impl::{apply_edit, diff};
use crate::pathguard::PathGuard;
use crate::sandbox::{BoxFut, FileOps};
use crate::s;
use anyhow::{Result, anyhow, bail};
use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::Value;
use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

pub const STALE: &str = "modified by another agent, re-read";

pub struct LocalFs {
    guard: Option<PathGuard>,
    cwd: Arc<Mutex<PathBuf>>,
    stamps: Mutex<HashMap<PathBuf, u64>>,
}

pub fn hash(b: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for x in b { h ^= *x as u64; h = h.wrapping_mul(0x100000001b3); }
    h
}

impl LocalFs {
    pub fn new(guard: Option<PathGuard>, cwd: Arc<Mutex<PathBuf>>) -> Self {
        LocalFs { guard, cwd, stamps: Default::default() }
    }

    fn resolve(&self, p: &str) -> PathBuf {
        let pb = Path::new(p);
        if pb.is_absolute() { pb.to_path_buf() } else { self.cwd.lock().unwrap().join(pb) }
    }

    fn open_read(&self, p: &Path) -> std::io::Result<File> {
        match &self.guard { Some(g) => g.open_read(p), None => File::open(p) }
    }

    fn canon(&self, f: &File, p: &Path) -> PathBuf {
        match &self.guard {
            Some(g) => g.canon_of(f).unwrap_or_else(|| p.to_path_buf()),
            None => p.canonicalize().unwrap_or_else(|_| p.to_path_buf()),
        }
    }

    /// whole file + canonical path
    fn read_bytes(&self, p: &Path, max: u64) -> Result<(Vec<u8>, PathBuf)> {
        let mut f = self.open_read(p).map_err(|e| anyhow!("{}", clean(e, p)))?;
        let meta = f.metadata()?;
        if meta.is_dir() { bail!("{} is a directory; use ls", p.display()); }
        if meta.len() > max { bail!("file too large ({} bytes)", meta.len()); }
        let mut b = Vec::with_capacity(meta.len() as usize);
        f.read_to_end(&mut b)?;
        let c = self.canon(&f, p);
        Ok((b, c))
    }

    fn write_bytes(&self, p: &Path, content: &[u8]) -> Result<PathBuf> {
        if let Some(d) = p.parent() {
            match &self.guard {
                Some(g) => g.mkdir_p(d).map_err(|e| anyhow!("{}", clean(e, p)))?,
                None => std::fs::create_dir_all(d)?,
            }
        }
        let mut f = match &self.guard { Some(g) => g.open_write(p), None => File::create(p) }.map_err(|e| anyhow!("{}", clean(e, p)))?;
        f.write_all(content)?;
        Ok(self.canon(&f, p))
    }

    fn note(&self, canon: PathBuf, content: &[u8]) { self.stamps.lock().unwrap().insert(canon, hash(content)); }

    fn read_i(&self, a: &Value) -> Result<String> {
        let p = self.resolve(s(a, "path")?);
        let (bytes, canon) = self.read_bytes(&p, 5_000_000)?;
        if bytes[..bytes.len().min(4096)].contains(&0) { bail!("binary file"); }
        self.note(canon, &bytes);
        let text = String::from_utf8_lossy(&bytes);
        let off = a["offset"].as_u64().unwrap_or(1).max(1) as usize;
        let lim = a["limit"].as_u64().unwrap_or(2000) as usize;
        let mut out = String::new();
        let total = text.lines().count();
        for (i, l) in text.lines().enumerate().skip(off - 1).take(lim) {
            let l = if l.len() > 2000 { &l[..l.floor_char_boundary(2000)] } else { l };
            out.push_str(&format!("{:>6}\t{}\n", i + 1, l));
        }
        if off - 1 + lim < total { out.push_str(&format!("... ({} more lines; use offset)\n", total - (off - 1 + lim))); }
        if out.is_empty() { out.push_str("(empty file)"); }
        Ok(out)
    }

    fn ls_i(&self, a: &Value) -> Result<String> {
        let p = self.resolve(a["path"].as_str().unwrap_or("."));
        let mut ents: Vec<(bool, String)> = match &self.guard {
            Some(g) => g.list_dir(&p).map_err(|e| anyhow!("{}", clean(e, &p)))?.into_iter().map(|(n, d)| (d, n)).collect(),
            None => std::fs::read_dir(&p).map_err(|e| anyhow!("{}: {e}", p.display()))?.flatten()
                .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().into_owned())).collect(),
        };
        ents.retain(|e| e.1 != ".git");
        ents.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
        Ok(ents.into_iter().map(|(d, n)| if d { format!("{n}/") } else { n }).collect::<Vec<_>>().join("\n"))
    }

    fn write_i(&self, a: &Value) -> Result<String> {
        let p = self.resolve(s(a, "path")?);
        let content = s(a, "content")?;
        match self.read_bytes(&p, 64_000_000) {
            Ok((cur, canon)) => {
                match self.stamps.lock().unwrap().get(&canon) {
                    None => bail!("file exists; read it first before overwriting"),
                    Some(h) if *h != hash(&cur) => bail!("{STALE}"),
                    _ => {}
                }
            }
            Err(e) => {
                let nf = e.to_string().contains("No such file");
                if !nf { return Err(e); }
            }
        }
        let canon = self.write_bytes(&p, content.as_bytes())?;
        self.note(canon, content.as_bytes());
        Ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
    }

    fn load(&self, a: &Value) -> Result<(PathBuf, PathBuf, String)> {
        let p = self.resolve(s(a, "path")?);
        let (bytes, canon) = self.read_bytes(&p, 64_000_000)?;
        match self.stamps.lock().unwrap().get(&canon) {
            None => bail!("read the file first"),
            Some(h) if *h != hash(&bytes) => bail!("{STALE}"),
            _ => {}
        }
        let text = String::from_utf8(bytes).map_err(|_| anyhow!("file is not valid UTF-8"))?;
        Ok((p, canon, text))
    }

    fn edit_i(&self, a: &Value) -> Result<String> {
        let (old, new) = (s(a, "old_string")?, s(a, "new_string")?);
        if old == new { bail!("old_string and new_string identical"); }
        let (p, _, text) = self.load(a)?;
        let (updated, n) = apply_edit(&text, old, new, a["replace_all"].as_bool().unwrap_or(false))?;
        let canon = self.write_bytes(&p, updated.as_bytes())?;
        self.note(canon, updated.as_bytes());
        Ok(format!("edited {} ({n} replacement{})\n{}", p.display(), if n == 1 { "" } else { "s" }, diff(&text, &updated)))
    }

    fn multiedit_i(&self, a: &Value) -> Result<String> {
        let edits = a["edits"].as_array().ok_or_else(|| anyhow!("edits must be an array"))?;
        if edits.is_empty() { bail!("edits is empty"); }
        let (p, _, text) = self.load(a)?;
        let mut cur = text.clone();
        for (i, e) in edits.iter().enumerate() {
            let (old, new) = (s(e, "old_string")?, s(e, "new_string")?);
            let (next, _) = apply_edit(&cur, old, new, e["replace_all"].as_bool().unwrap_or(false))
                .map_err(|err| anyhow!("edit #{} failed (nothing written): {err}", i + 1))?;
            cur = next;
        }
        let canon = self.write_bytes(&p, cur.as_bytes())?;
        self.note(canon, cur.as_bytes());
        Ok(format!("edited {} ({} edits)\n{}", p.display(), edits.len(), diff(&text, &cur)))
    }

    fn check_root(&self, root: &Path) -> Result<()> {
        if let Some(g) = &self.guard { g.open_dir(root).map_err(|e| anyhow!("{}", clean(e, root)))?; }
        Ok(())
    }

    fn glob_i(&self, a: &Value) -> Result<String> {
        let root = self.resolve(a["path"].as_str().unwrap_or("."));
        self.check_root(&root)?;
        let m = Glob::new(s(a, "pattern")?)?.compile_matcher();
        let mut hits: Vec<(std::time::SystemTime, String)> = vec![];
        for e in WalkBuilder::new(&root).build().flatten() {
            if !e.file_type().is_some_and(|t| t.is_file()) { continue; }
            let rel = e.path().strip_prefix(&root).unwrap_or(e.path());
            if m.is_match(rel) || (!s(a, "pattern")?.contains('/') && e.path().file_name().is_some_and(|n| m.is_match(n))) {
                let mt = e.metadata().ok().and_then(|m| m.modified().ok()).unwrap_or(std::time::UNIX_EPOCH);
                hits.push((mt, rel.display().to_string()));
            }
        }
        hits.sort_by_key(|h| std::cmp::Reverse(h.0));
        let total = hits.len();
        let mut out: Vec<String> = hits.into_iter().take(500).map(|h| h.1).collect();
        if total > 500 { out.push(format!("... {} more", total - 500)); }
        Ok(if out.is_empty() { "no matches".into() } else { out.join("\n") })
    }

    fn grep_i(&self, a: &Value) -> Result<String> {
        let root = self.resolve(a["path"].as_str().unwrap_or("."));
        self.check_root(&root)?;
        let re = RegexBuilder::new(s(a, "pattern")?).size_limit(10_000_000).build()?;
        let inc = a["include"].as_str().map(Glob::new).transpose()?.map(|g| g.compile_matcher());
        let ctxn = a["context"].as_u64().unwrap_or(0) as usize;
        let limit = a["limit"].as_u64().unwrap_or(200) as usize;
        let mut out = vec![];
        let mut count = 0;
        'files: for e in WalkBuilder::new(&root).build().flatten() {
            if !e.file_type().is_some_and(|t| t.is_file()) { continue; }
            if let Some(m) = &inc
                && !e.path().file_name().is_some_and(|n| m.is_match(n)) { continue; }
            let bytes = match &self.guard {
                Some(g) => match g.open_read(e.path()) {
                    Ok(mut f) => { let mut b = vec![]; if f.read_to_end(&mut b).is_err() { continue; } b }
                    Err(_) => continue,
                },
                None => match std::fs::read(e.path()) { Ok(b) => b, Err(_) => continue },
            };
            if bytes[..bytes.len().min(2048)].contains(&0) { continue; }
            let text = String::from_utf8_lossy(&bytes);
            let lines: Vec<&str> = text.lines().collect();
            let rel = e.path().strip_prefix(&root).unwrap_or(e.path()).display().to_string();
            for (i, l) in lines.iter().enumerate() {
                if re.is_match(l) {
                    let (lo, hi) = (i.saturating_sub(ctxn), (i + ctxn).min(lines.len() - 1));
                    for (j, l) in lines.iter().enumerate().take(hi + 1).skip(lo) {
                        out.push(format!("{rel}:{}{}{}", j + 1, if j == i { ":" } else { "-" }, &l[..l.floor_char_boundary(300)]));
                    }
                    count += 1;
                    if count >= limit { out.push(format!("... limit {limit} reached")); break 'files; }
                }
            }
        }
        Ok(if out.is_empty() { "no matches".into() } else { out.join("\n") })
    }
}

fn clean(e: std::io::Error, p: &Path) -> String {
    let m = e.to_string();
    if m.contains(&p.display().to_string()) { m } else { format!("{}: {m}", p.display()) }
}

impl FileOps for LocalFs {
    fn read<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.read_i(a) }) }
    fn write<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.write_i(a) }) }
    fn edit<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.edit_i(a) }) }
    fn multiedit<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.multiedit_i(a) }) }
    fn glob<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.glob_i(a) }) }
    fn grep<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.grep_i(a) }) }
    fn ls<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(async move { self.ls_i(a) }) }
}
