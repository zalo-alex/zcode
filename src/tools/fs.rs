use super::{Ctx, s};
use anyhow::{Result, bail};
use serde_json::Value;

pub async fn read(a: &Value, ctx: &Ctx) -> Result<String> {
    let p = ctx.resolve(s(a, "path")?);
    let meta = tokio::fs::metadata(&p).await.map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
    if meta.is_dir() { bail!("{} is a directory; use ls", p.display()); }
    if meta.len() > 5_000_000 { bail!("file too large ({} bytes)", meta.len()); }
    let bytes = tokio::fs::read(&p).await?;
    if bytes[..bytes.len().min(4096)].contains(&0) { bail!("binary file"); }
    let text = String::from_utf8_lossy(&bytes);
    ctx.read_files.lock().unwrap().insert(p.canonicalize().unwrap_or(p));
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

pub fn ls(a: &Value, ctx: &Ctx) -> Result<String> {
    let p = ctx.resolve(a["path"].as_str().unwrap_or("."));
    let mut ents: Vec<(bool, String)> = std::fs::read_dir(&p)
        .map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?
        .flatten()
        .filter(|e| e.file_name() != ".git")
        .map(|e| (e.path().is_dir(), e.file_name().to_string_lossy().into_owned()))
        .collect();
    ents.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    Ok(ents.into_iter().map(|(d, n)| if d { format!("{n}/") } else { n }).collect::<Vec<_>>().join("\n"))
}

pub async fn write(a: &Value, ctx: &Ctx) -> Result<String> {
    let p = ctx.resolve(s(a, "path")?);
    let content = s(a, "content")?;
    if p.exists() {
        let c = p.canonicalize()?;
        if !ctx.read_files.lock().unwrap().contains(&c) {
            bail!("file exists; read it first before overwriting");
        }
    }
    if let Some(d) = p.parent() { tokio::fs::create_dir_all(d).await?; }
    tokio::fs::write(&p, content).await?;
    ctx.read_files.lock().unwrap().insert(p.canonicalize().unwrap_or(p.clone()));
    Ok(format!("wrote {} ({} bytes)", p.display(), content.len()))
}

pub fn todo_read(ctx: &Ctx) -> String {
    let t = ctx.todos.lock().unwrap();
    if t.is_empty() { return "(no todos)".into(); }
    t.iter().map(|(s, c)| format!("[{}] {}", match s.as_str() { "completed" => "x", "in_progress" => "~", _ => " " }, c)).collect::<Vec<_>>().join("\n")
}

pub fn todo(a: &Value, ctx: &Ctx) -> Result<String> {
    let items = a["todos"].as_array().ok_or_else(|| anyhow::anyhow!("todos must be array"))?;
    let v: Vec<(String, String)> = items.iter()
        .map(|t| (t["status"].as_str().unwrap_or("pending").into(), t["content"].as_str().unwrap_or("").into()))
        .collect();
    let out = v.iter().map(|(s, c)| format!("[{}] {}", match s.as_str() { "completed" => "x", "in_progress" => "~", _ => " " }, c)).collect::<Vec<_>>().join("\n");
    *ctx.todos.lock().unwrap() = v;
    Ok(out)
}
