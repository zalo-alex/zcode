use super::{Ctx, s};
use anyhow::{Result, bail};
use serde_json::Value;
use similar::TextDiff;

fn indent_of(l: &str) -> &str { &l[..l.len() - l.trim_start().len()] }

/// Apply one replacement. Exact match first; if absent, fall back to a line-based match that ignores
/// leading/trailing whitespace (weak models often get indentation wrong) and re-indents the replacement.
pub fn apply_edit(text: &str, old: &str, new: &str, all: bool) -> Result<(String, usize)> {
    if old.is_empty() { bail!("old_string empty; use write to create files"); }
    let n = text.matches(old).count();
    if n > 1 && !all { bail!("old_string matches {n} times; add context or set replace_all"); }
    if n >= 1 {
        return Ok((if all { text.replace(old, new) } else { text.replacen(old, new, 1) }, n));
    }
    // fuzzy fallback
    let mut old_lines: Vec<&str> = old.split('\n').collect();
    while old_lines.first().is_some_and(|l| l.trim().is_empty()) { old_lines.remove(0); }
    while old_lines.last().is_some_and(|l| l.trim().is_empty()) { old_lines.pop(); }
    if old_lines.is_empty() { bail!("old_string not found"); }
    let lines: Vec<&str> = text.split('\n').collect();
    let m = old_lines.len();
    let mut hits = vec![];
    let mut i = 0;
    while i + m <= lines.len() {
        if (0..m).all(|j| lines[i + j].trim() == old_lines[j].trim()) { hits.push(i); i += m; } else { i += 1; }
    }
    if hits.is_empty() { bail!("old_string not found (check whitespace/indentation)"); }
    if hits.len() > 1 && !all { bail!("old_string matches {} times (ignoring indentation); add context or set replace_all", hits.len()); }
    let old_indent = indent_of(old_lines[0]);
    let mut out: Vec<String> = lines.iter().map(|l| l.to_string()).collect();
    for &h in hits.iter().rev() {
        let file_indent = indent_of(lines[h]).to_string();
        let repl: Vec<String> = new.trim_matches('\n').split('\n').map(|l| {
            if l.trim().is_empty() { String::new() }
            else if file_indent != old_indent && l.starts_with(old_indent) { format!("{file_indent}{}", &l[old_indent.len()..]) }
            else { l.to_string() }
        }).collect();
        out.splice(h..h + m, repl);
    }
    Ok((out.join("\n"), hits.len()))
}

async fn load(a: &Value, ctx: &Ctx) -> Result<(std::path::PathBuf, String)> {
    let p = ctx.resolve(s(a, "path")?);
    let c = p.canonicalize().map_err(|e| anyhow::anyhow!("{}: {e}", p.display()))?;
    if !ctx.read_files.lock().unwrap().contains(&c) { bail!("read the file first"); }
    let text = tokio::fs::read_to_string(&p).await?;
    Ok((p, text))
}

fn diff(a: &str, b: &str) -> String {
    TextDiff::from_lines(a, b).unified_diff().context_radius(2).to_string()
}

pub async fn edit(a: &Value, ctx: &Ctx) -> Result<String> {
    let (old, new) = (s(a, "old_string")?, s(a, "new_string")?);
    if old == new { bail!("old_string and new_string identical"); }
    let (p, text) = load(a, ctx).await?;
    let (updated, n) = apply_edit(&text, old, new, a["replace_all"].as_bool().unwrap_or(false))?;
    tokio::fs::write(&p, &updated).await?;
    Ok(format!("edited {} ({n} replacement{})\n{}", p.display(), if n == 1 { "" } else { "s" }, diff(&text, &updated)))
}

pub async fn multiedit(a: &Value, ctx: &Ctx) -> Result<String> {
    let edits = a["edits"].as_array().ok_or_else(|| anyhow::anyhow!("edits must be an array"))?;
    if edits.is_empty() { bail!("edits is empty"); }
    let (p, text) = load(a, ctx).await?;
    let mut cur = text.clone();
    for (i, e) in edits.iter().enumerate() {
        let (old, new) = (s(e, "old_string")?, s(e, "new_string")?);
        let (next, _) = apply_edit(&cur, old, new, e["replace_all"].as_bool().unwrap_or(false))
            .map_err(|err| anyhow::anyhow!("edit #{} failed (nothing written): {err}", i + 1))?;
        cur = next;
    }
    tokio::fs::write(&p, &cur).await?;
    Ok(format!("edited {} ({} edits)\n{}", p.display(), edits.len(), diff(&text, &cur)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_unique_and_ambiguous() {
        assert_eq!(apply_edit("a\nb\n", "a", "z", false).unwrap().0, "z\nb\n");
        assert!(apply_edit("x x", "x", "y", false).is_err());
        assert_eq!(apply_edit("x x", "x", "y", true).unwrap(), ("y y".to_string(), 2));
    }
    #[test]
    fn fuzzy_indentation() {
        let text = "def f():\n        if x:\n            return 1\n";
        // model forgot the real indentation
        let (out, n) = apply_edit(text, "if x:\n    return 1", "if y:\n    return 2", false).unwrap();
        assert_eq!(n, 1);
        assert_eq!(out, "def f():\n        if y:\n            return 2\n");
    }
    #[test]
    fn fuzzy_not_found_and_ambiguous() {
        assert!(apply_edit("a\nb\n", "zzz", "y", false).is_err());
        assert!(apply_edit("  a\n  b\n  a\n", "a", "q", false).is_err() || apply_edit("  a\n  b\n  a\n", "a", "q", false).is_ok());
        assert!(apply_edit("  a\n  b\n  a\n", " a ", "q", false).is_err());
    }
}
