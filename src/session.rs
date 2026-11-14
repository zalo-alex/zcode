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
            {
                let mut first = String::new();
                std::io::BufRead::read_line(&mut std::io::BufReader::new(std::fs::File::open(&path)?), &mut first)?;
                Some(first.lines().next().unwrap_or("").to_string())
            }.filter(|l| l.contains("zcode_session"))
                .unwrap_or_else(|| header_of(&id, cwd, model))
        } else {
            let h = header_of(&id, cwd, model);
            std::fs::write(&path, format!("{h}\n"))?;
            h
        };
        Ok(Session { id, path, header })
    }


    /// Per-agent session `<base>.<agent>` for swarm workers (the `.` keeps it out of user-supplied ids).
    pub fn open_worker(base: &str, agent: &str, cwd: &Path, model: &str) -> Result<Session> {
        if !valid_id(agent) || agent.len() > 32 { bail!("invalid agent name `{agent}`"); }
        let id = format!("{base}.{agent}");
        std::fs::create_dir_all(dir())?;
        let path = dir().join(format!("{id}.jsonl"));
        let header = header_of(&id, cwd, model);
        if !path.exists() { std::fs::write(&path, format!("{header}\n"))?; }
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

/// Stored conversation of a session id, without creating or touching anything (`None` when the id is invalid or has no file).
pub fn load_by_id(id: &str) -> Option<Vec<Message>> {
    if !valid_id(id) && !id.split_once('.').is_some_and(|(b, a)| valid_id(b) && valid_id(a)) { return None; }
    let text = std::fs::read_to_string(dir().join(format!("{id}.jsonl"))).ok()?;
    Some(text.lines().filter(|l| !l.contains("\"zcode_session\"")).filter_map(|l| serde_json::from_str(l).ok()).collect())
}

/// Longest text/output/input string a history item carries.
pub const ITEM_CAP: usize = 12000;

/// `(s, false)` when it fits, else its first `ITEM_CAP` chars and `true`.
fn cut(s: &str) -> (String, bool) {
    if s.len() <= ITEM_CAP { return (s.to_string(), false); }
    match s.char_indices().nth(ITEM_CAP) { Some((i, _)) => (s[..i].to_string(), true), None => (s.to_string(), false) }
}

fn cut_value(v: &Value, any: &mut bool) -> Value {
    match v {
        Value::String(s) => { let (c, was) = cut(s); *any |= was; Value::String(c) }
        Value::Array(a) => Value::Array(a.iter().map(|x| cut_value(x, any)).collect()),
        Value::Object(o) => Value::Object(o.iter().map(|(k, x)| (k.clone(), cut_value(x, any))).collect()),
        _ => v.clone(),
    }
}

/// The conversation as UI items: `{k:"user",text}`, `{k:"text",text}`, `{k:"tool",id,tool,input,output,ok}`.
/// Tool results are paired with their calls by id (in order when an id does not match); a call without a result yet has
/// `output` and `ok` null. Anything cut to `ITEM_CAP` chars gets `"cut": true`. System messages are not items.
pub fn items(msgs: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = vec![];
    let mut pending: Vec<(usize, String)> = vec![]; // (index in `out`, call id) still waiting for a result
    for m in msgs {
        match m.role.as_str() {
            "user" | "assistant" => {
                let k = if m.role == "user" { "user" } else { "text" };
                if let Some(c) = m.content.as_deref().filter(|c| !c.is_empty()) {
                    let (text, was) = cut(c);
                    let mut it = json!({"k": k, "text": text});
                    if was { it["cut"] = json!(true); }
                    out.push(it);
                }
                for c in m.tool_calls.iter().flatten() {
                    let mut was = false;
                    let input = serde_json::from_str::<Value>(&c.function.arguments).map(|v| cut_value(&v, &mut was)).unwrap_or_else(|_| { let (s, w) = cut(&c.function.arguments); was = w; Value::String(s) });
                    let mut it = json!({"k": "tool", "id": c.id, "tool": c.function.name, "input": input, "output": Value::Null, "ok": Value::Null});
                    if was { it["cut"] = json!(true); }
                    pending.push((out.len(), c.id.clone()));
                    out.push(it);
                }
            }
            "tool" => {
                let id = m.tool_call_id.as_deref().unwrap_or("");
                let slot = pending.iter().position(|p| p.1 == id).or(if pending.is_empty() { None } else { Some(0) });
                let Some(s) = slot else { continue };
                let (i, _) = pending.remove(s);
                let body = m.content.as_deref().unwrap_or("");
                let (output, was) = cut(body);
                out[i]["output"] = json!(output);
                out[i]["ok"] = json!(!body.starts_with("Error:"));
                if was { out[i]["cut"] = json!(true); }
            }
            _ => {}
        }
    }
    out
}

/// `want_count`: exact message count (streams the file); otherwise only "0 or 1+" (reads two lines).
fn read_info(p: &Path, mtime: std::time::SystemTime, want_count: bool) -> Option<Info> {
    use std::io::{BufRead, BufReader};
    let mut r = BufReader::new(std::fs::File::open(p).ok()?);
    let mut first = String::new();
    r.read_line(&mut first).ok()?;
    let h: Value = serde_json::from_str(first.lines().next()?).ok()?;
    let h = h.get("zcode_session")?;
    let mut messages = 0;
    if want_count {
        let mut buf = [0u8; 64 * 1024];
        let mut last = b'\n';
        // lines after the header: count newlines, plus a final unterminated line
        loop {
            let n = std::io::Read::read(&mut r, &mut buf).ok()?;
            if n == 0 { break; }
            messages += buf[..n].iter().filter(|&&b| b == b'\n').count();
            last = buf[n - 1];
        }
        if last != b'\n' { messages += 1; }
        if !first.ends_with('\n') { messages = 0; }
    } else {
        let mut rest = String::new();
        if r.read_line(&mut rest).ok()? > 0 { messages = 1; }
    }
    Some(Info {
        id: h["id"].as_str()?.to_string(),
        cwd: h["cwd"].as_str().unwrap_or("").to_string(),
        model: h["model"].as_str().unwrap_or("").to_string(),
        mtime,
        messages,
    })
}

/// Session files newest first, by mtime only (no file contents read).
fn files_by_mtime() -> Vec<(PathBuf, std::time::SystemTime)> {
    let mut v: Vec<_> = std::fs::read_dir(dir()).into_iter().flatten().flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x == "jsonl"))
        .filter_map(|e| Some((e.path(), e.metadata().ok()?.modified().ok()?))).collect();
    v.sort_by_key(|f| std::cmp::Reverse(f.1));
    v
}

/// The `n` most recently updated sessions; only those files are read.
pub fn list_top(n: usize) -> Vec<Info> {
    files_by_mtime().into_iter().filter_map(|(p, m)| read_info(&p, m, true)).take(n).collect()
}

/// Most recently updated session whose working directory is `cwd`.
pub fn latest_for_cwd(cwd: &Path) -> Option<String> {
    let want = cwd.display().to_string();
    files_by_mtime().into_iter().filter_map(|(p, m)| read_info(&p, m, false))
        .find(|i| i.cwd == want && i.messages > 0 && !i.id.contains('.')).map(|i| i.id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(id: &str, name: &str, args: &str) -> crate::llm::ToolCall { crate::llm::ToolCall { id: id.into(), kind: "function".into(), function: crate::llm::FnCall { name: name.into(), arguments: args.into() } } }

    #[test]
    fn items_pair_results_by_id_then_by_order_and_cut_big_texts() {
        let mut a = Message::text("assistant", "looking");
        a.tool_calls = Some(vec![call("c1", "read", "{\"path\":\"a\"}"), call("c2", "bash", "{\"command\":\"ls\"}"), call("c3", "ls", "not json")]);
        let big = "y".repeat(ITEM_CAP + 5);
        let msgs = vec![Message::text("system", "sys"), Message::text("user", "go"), a, Message::tool("c2", "Error: boom"), Message::tool("c1", big.as_str()), Message::tool("weird-id", "by order")];
        let it = items(&msgs);
        assert_eq!(it.iter().map(|i| i["k"].as_str().unwrap()).collect::<Vec<_>>(), ["user", "text", "tool", "tool", "tool"]);
        assert_eq!((it[2]["id"].as_str(), it[2]["ok"].as_bool(), it[2]["cut"].as_bool()), (Some("c1"), Some(true), Some(true)));
        assert_eq!(it[2]["output"].as_str().unwrap().len(), ITEM_CAP);
        assert_eq!((it[3]["output"].as_str(), it[3]["ok"].as_bool()), (Some("Error: boom"), Some(false)));
        assert_eq!((it[4]["output"].as_str(), it[4]["input"].as_str()), (Some("by order"), Some("not json")));
        assert_eq!(it[2]["input"]["path"], "a");
        // a call with no result yet
        let mut b = Message::text("assistant", "");
        b.tool_calls = Some(vec![call("p", "bash", "{}")]);
        let it = items(&[b]);
        assert!(it[0]["output"].is_null() && it[0]["ok"].is_null());
    }

    #[test]
    fn load_by_id_rejects_bad_ids() {
        assert!(load_by_id("../x").is_none() && load_by_id("").is_none() && load_by_id("a/b.c").is_none());
    }

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
