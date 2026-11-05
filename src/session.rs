//! Append-only JSONL sessions: line 1 = header, then one `Message` per line.
//! Crash-safe (the process may be killed at any time).
use crate::llm::Message;
use anyhow::{Result, bail};
use serde_json::{Value, json};
use std::io::Write;
use std::path::{Path, PathBuf};

pub fn dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".local/share"));
    base.join("zcode/sessions")
}

pub fn new_id() -> String {
    let d = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap();
    // nanos mixed with pid: unique enough without a rand dependency
    let r = (d.subsec_nanos() as u64 ^ ((std::process::id() as u64) << 20)) & 0xff_ffff;
    format!("ses_{:x}{:06x}", d.as_secs(), r)
}

fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

pub struct Session {
    pub id: String,
    path: PathBuf,
    header: String,
}

pub struct Info {
    pub id: String,
    pub cwd: String,
    pub model: String,
    pub mtime: std::time::SystemTime,
    pub messages: usize,
}

fn header_of(id: &str, cwd: &Path, model: &str) -> String {
    json!({"zcode_session": {"id": id, "cwd": cwd.display().to_string(), "model": model,
        "created": std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)}}).to_string()
}

impl Session {
    /// Open an existing session by id, or create it (with that id, or a fresh one).
    pub fn open_or_create(id: Option<&str>, cwd: &Path, model: &str) -> Result<Session> {
        let id = match id {
            Some(i) if valid_id(i) => i.to_string(),
            Some(i) => bail!("invalid session id `{i}` (allowed: letters, digits, _ and -)"),
            None => new_id(),
        };
        std::fs::create_dir_all(dir())?;
        let path = dir().join(format!("{id}.jsonl"));
        let header = if path.exists() {
            std::fs::read_to_string(&path)?.lines().next().filter(|l| l.contains("zcode_session")).map(String::from)
                .unwrap_or_else(|| header_of(&id, cwd, model))
        } else {
            let h = header_of(&id, cwd, model);
            std::fs::write(&path, format!("{h}\n"))?;
            h
        };
        Ok(Session { id, path, header })
    }

    pub fn load(&self) -> Vec<Message> {
        std::fs::read_to_string(&self.path).unwrap_or_default().lines()
            .filter(|l| !l.contains("\"zcode_session\""))
            .filter_map(|l| serde_json::from_str(l).ok()).collect()
    }

    pub fn append(&self, m: &Message) {
        if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(&self.path) {
            let _ = writeln!(f, "{}", serde_json::to_string(m).unwrap_or_default());
        }
    }

    /// Replace the stored conversation (after compaction / clear).
    pub fn rewrite(&self, msgs: &[Message]) {
        let mut s = format!("{}\n", self.header);
        for m in msgs {
            s.push_str(&serde_json::to_string(m).unwrap_or_default());
            s.push('\n');
        }
        let tmp = self.path.with_extension("tmp");
        if std::fs::write(&tmp, s).is_ok() {
            let _ = std::fs::rename(&tmp, &self.path);
        }
    }
}

fn read_info(p: &Path) -> Option<Info> {
    let text = std::fs::read_to_string(p).ok()?;
    let h: Value = serde_json::from_str(text.lines().next()?).ok()?;
    let h = h.get("zcode_session")?;
    Some(Info {
        id: h["id"].as_str()?.to_string(),
        cwd: h["cwd"].as_str().unwrap_or("").to_string(),
        model: h["model"].as_str().unwrap_or("").to_string(),
        mtime: std::fs::metadata(p).ok()?.modified().ok()?,
        messages: text.lines().count().saturating_sub(1),
    })
}

pub fn list() -> Vec<Info> {
    let mut v: Vec<Info> = std::fs::read_dir(dir()).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| read_info(&e.path())).collect();
    v.sort_by_key(|i| std::cmp::Reverse(i.mtime));
    v
}

/// Most recently updated session whose working directory is `cwd`.
pub fn latest_for_cwd(cwd: &Path) -> Option<String> {
    let want = cwd.display().to_string();
    list().into_iter().find(|i| i.cwd == want && i.messages > 0).map(|i| i.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_validation_blocks_traversal() {
        assert!(valid_id("ses_abc-1") && !valid_id("../x") && !valid_id("") && !valid_id("a/b"));
    }
    #[test]
    fn ids_look_right() {
        let a = new_id();
        assert!(a.starts_with("ses_") && valid_id(&a));
    }
}
