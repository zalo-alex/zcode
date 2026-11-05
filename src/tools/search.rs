use super::{Ctx, s};
use anyhow::Result;
use globset::Glob;
use ignore::WalkBuilder;
use regex::RegexBuilder;
use serde_json::Value;

pub fn glob(a: &Value, ctx: &Ctx) -> Result<String> {
    let root = ctx.resolve(a["path"].as_str().unwrap_or("."));
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

pub fn grep(a: &Value, ctx: &Ctx) -> Result<String> {
    let root = ctx.resolve(a["path"].as_str().unwrap_or("."));
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
        let Ok(bytes) = std::fs::read(e.path()) else { continue };
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
