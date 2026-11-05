mod bash;
mod edit;
mod fs;
mod search;
mod web;

use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

pub struct Ctx {
    pub cwd: Mutex<PathBuf>,
    pub read_files: Mutex<HashSet<PathBuf>>,
    pub todos: Mutex<Vec<(String, String)>>, // (status, content)
}

impl Ctx {
    pub fn new(cwd: PathBuf) -> Self {
        Ctx { cwd: Mutex::new(cwd), read_files: Default::default(), todos: Default::default() }
    }
    pub fn cwd(&self) -> PathBuf { self.cwd.lock().unwrap().clone() }
    pub fn resolve(&self, p: &str) -> PathBuf {
        let pb = Path::new(p);
        if pb.is_absolute() { pb.to_path_buf() } else { self.cwd().join(pb) }
    }
}

pub struct Spec {
    pub name: &'static str,
    pub desc: &'static str,
    pub params: fn() -> Value,
    pub readonly: bool,
}

fn obj(props: Value, req: &[&str]) -> Value {
    json!({"type":"object","properties":props,"required":req})
}

pub const SPECS: &[Spec] = &[
    Spec { name: "read", readonly: true,
        desc: "Read a text file with line numbers. Use offset/limit for big files.",
        params: || obj(json!({"path":{"type":"string"},"offset":{"type":"integer","description":"1-based start line"},"limit":{"type":"integer"}}), &["path"]) },
    Spec { name: "ls", readonly: true,
        desc: "List a directory (default cwd), respecting .gitignore.",
        params: || obj(json!({"path":{"type":"string"}}), &[]) },
    Spec { name: "glob", readonly: true,
        desc: "Find files by glob pattern (e.g. **/*.rs), newest first.",
        params: || obj(json!({"pattern":{"type":"string"},"path":{"type":"string"}}), &["pattern"]) },
    Spec { name: "grep", readonly: true,
        desc: "Regex search file contents. Returns file:line:text.",
        params: || obj(json!({"pattern":{"type":"string"},"path":{"type":"string"},"include":{"type":"string","description":"glob filter e.g. *.rs"},"context":{"type":"integer"},"limit":{"type":"integer","description":"max matches (default 200)"}}), &["pattern"]) },
    Spec { name: "write", readonly: false,
        desc: "Create or overwrite a file. Existing files must be read first.",
        params: || obj(json!({"path":{"type":"string"},"content":{"type":"string"}}), &["path","content"]) },
    Spec { name: "edit", readonly: false,
        desc: "Replace exact old_string with new_string in a file (must be unique unless replace_all). Read file first.",
        params: || obj(json!({"path":{"type":"string"},"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}}), &["path","old_string","new_string"]) },
    Spec { name: "multiedit", readonly: false,
        desc: "Apply several edits to one file atomically (all or nothing). Each edit: {old_string,new_string,replace_all?}.",
        params: || obj(json!({"path":{"type":"string"},"edits":{"type":"array","items":{"type":"object","properties":{"old_string":{"type":"string"},"new_string":{"type":"string"},"replace_all":{"type":"boolean"}},"required":["old_string","new_string"]}}}), &["path","edits"]) },
    Spec { name: "bash", readonly: false,
        desc: "Run a shell command (sh -c); the working directory persists between calls. Default timeout 120s. Use for builds, tests, git. Use background:true for servers/watchers.",
        params: || obj(json!({"command":{"type":"string"},"timeout":{"type":"integer","description":"seconds (default 120)"},"background":{"type":"boolean","description":"run detached (dev servers, watchers); returns a job id"}}), &["command"]) },
    Spec { name: "bash_output", readonly: true,
        desc: "Status and recent output of background jobs. Without id: list jobs.",
        params: || obj(json!({"id":{"type":"string"},"lines":{"type":"integer"}}), &[]) },
    Spec { name: "bash_kill", readonly: false,
        desc: "Stop a background job started with bash background:true.",
        params: || obj(json!({"id":{"type":"string"}}), &["id"]) },
    Spec { name: "webfetch", readonly: true,
        desc: "Fetch a URL and return text content (HTML stripped).",
        params: || obj(json!({"url":{"type":"string"}}), &["url"]) },
    Spec { name: "task", readonly: true,
        desc: "Delegate a self-contained research/exploration job to a sub-agent with read-only tools. Returns its final report. Use for broad codebase searches.",
        params: || obj(json!({"prompt":{"type":"string","description":"complete instructions, sub-agent has no other context"}}), &["prompt"]) },
    Spec { name: "todoread", readonly: true,
        desc: "Read the current todo list.",
        params: || obj(json!({}), &[]) },
    Spec { name: "todowrite", readonly: true,
        desc: "Replace the todo list. Each item: {content, status: pending|in_progress|completed}.",
        params: || obj(json!({"todos":{"type":"array","items":{"type":"object","properties":{"content":{"type":"string"},"status":{"type":"string"}},"required":["content","status"]}}}), &["todos"]) },
];

pub fn schemas(readonly_only: bool) -> Vec<Value> {
    SPECS.iter()
        .filter(|s| !readonly_only || s.readonly)
        .map(|s| json!({"type":"function","function":{"name":s.name,"description":s.desc,"parameters":(s.params)()}}))
        .collect()
}

pub fn is_readonly(name: &str) -> bool {
    SPECS.iter().find(|s| s.name == name).map(|s| s.readonly).unwrap_or(false)
}

pub fn s<'a>(a: &'a Value, k: &str) -> Result<&'a str> {
    a.get(k).and_then(|v| v.as_str()).ok_or_else(|| anyhow!("missing string arg `{k}`"))
}

/// Cap output; keep head+tail, spill nothing (simple).
pub fn cap(out: String, max: usize) -> String {
    if out.len() <= max { return out; }
    let h = max * 2 / 3;
    let t = max - h;
    let hs = floor(&out, h);
    let ts = ceil(&out, out.len() - t);
    format!("{}\n... [{} bytes truncated] ...\n{}", &out[..hs], ts - hs, &out[ts..])
}
fn floor(s: &str, mut i: usize) -> usize { while !s.is_char_boundary(i) { i -= 1; } i }
fn ceil(s: &str, mut i: usize) -> usize { while !s.is_char_boundary(i) { i += 1; } i }

pub async fn run(name: &str, args: &Value, ctx: &Ctx) -> Result<String> {
    let out = match name {
        "read" => fs::read(args, ctx).await?,
        "ls" => fs::ls(args, ctx)?,
        "write" => fs::write(args, ctx).await?,
        "edit" => edit::edit(args, ctx).await?,
        "glob" => search::glob(args, ctx)?,
        "grep" => search::grep(args, ctx)?,
        "bash" => bash::bash(args, ctx).await?,
        "bash_output" => bash::output(args)?,
        "bash_kill" => bash::kill_job(args).await?,
        "multiedit" => edit::multiedit(args, ctx).await?,
        "todoread" => fs::todo_read(ctx),
        "webfetch" => web::fetch(args).await?,
        "todowrite" => fs::todo(args, ctx)?,
        "task" => return Err(anyhow!("task handled by agent")),
        _ => return Err(anyhow!("unknown tool `{name}`")),
    };
    Ok(cap_spill(out, 30_000))
}

/// Like `cap`, but when output is cut the full text is saved to a file whose path is appended.
fn cap_spill(out: String, max: usize) -> String {
    if out.len() <= max { return out; }
    let dir = bash::cache_dir().join("tool-output");
    let path = dir.join(format!("{}-{}.txt", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)));
    let saved = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, &out)).is_ok();
    let mut c = cap(out, max);
    if saved { c.push_str(&format!("\n[full output saved to {}; use read with offset/limit or grep]", path.display())); }
    c
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zcode_t_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[tokio::test]
    async fn edit_rules() {
        let d = tmp("edit");
        std::fs::write(d.join("a.txt"), "x\nfoo\nfoo\n").unwrap();
        let ctx = Ctx::new(d.clone());
        // must read first
        assert!(run("edit", &json!({"path":"a.txt","old_string":"x","new_string":"y"}), &ctx).await.is_err());
        run("read", &json!({"path":"a.txt"}), &ctx).await.unwrap();
        // ambiguous
        assert!(run("edit", &json!({"path":"a.txt","old_string":"foo","new_string":"bar"}), &ctx).await.is_err());
        run("edit", &json!({"path":"a.txt","old_string":"foo","new_string":"bar","replace_all":true}), &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "x\nbar\nbar\n");
        assert!(run("edit", &json!({"path":"a.txt","old_string":"nope","new_string":"z"}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn write_requires_read_for_existing() {
        let d = tmp("write");
        let ctx = Ctx::new(d.clone());
        run("write", &json!({"path":"n/new.txt","content":"hi"}), &ctx).await.unwrap();
        assert!(run("write", &json!({"path":"n/new.txt","content":"x"}), &ctx).await.is_ok()); // created by us => tracked
        std::fs::write(d.join("old.txt"), "o").unwrap();
        assert!(run("write", &json!({"path":"old.txt","content":"x"}), &ctx).await.is_err());
    }

    #[tokio::test]
    async fn grep_glob_ls() {
        let d = tmp("search");
        std::fs::create_dir_all(d.join("src")).unwrap();
        std::fs::write(d.join("src/a.rs"), "fn main() {}\nlet needle = 1;\n").unwrap();
        std::fs::write(d.join("b.txt"), "nothing\n").unwrap();
        let ctx = Ctx::new(d);
        let g = run("grep", &json!({"pattern":"needle"}), &ctx).await.unwrap();
        assert!(g.contains("src/a.rs:2:"), "{g}");
        let g = run("grep", &json!({"pattern":"needle","include":"*.txt"}), &ctx).await.unwrap();
        assert_eq!(g, "no matches");
        assert!(run("glob", &json!({"pattern":"**/*.rs"}), &ctx).await.unwrap().contains("src/a.rs"));
        assert!(run("glob", &json!({"pattern":"*.rs"}), &ctx).await.unwrap().contains("a.rs"));
        let l = run("ls", &json!({}), &ctx).await.unwrap();
        assert!(l.starts_with("src/") && l.contains("b.txt"));
    }

    #[tokio::test]
    async fn bash_exit_and_timeout() {
        let ctx = Ctx::new(tmp("bash"));
        let o = run("bash", &json!({"command":"echo hi; echo err >&2; exit 3"}), &ctx).await.unwrap();
        assert!(o.contains("hi") && o.contains("err") && o.contains("[exit 3]"));
        let t = std::time::Instant::now();
        let o = run("bash", &json!({"command":"sleep 30","timeout":1}), &ctx).await.unwrap();
        assert!(o.contains("timeout") && t.elapsed().as_secs() < 5);
    }

    #[test]
    fn cap_keeps_ends_and_utf8() {
        let s = "é".repeat(5000);
        let c = cap(s.clone(), 1000);
        assert!(c.len() < 1200 && c.contains("truncated") && c.starts_with('é') && c.ends_with('é'));
        assert_eq!(cap("short".into(), 100), "short");
    }

    #[tokio::test]
    async fn bash_cd_persists_and_background_returns_fast() {
        let d = tmp("bashcd");
        std::fs::create_dir_all(d.join("sub")).unwrap();
        let ctx = Ctx::new(d.canonicalize().unwrap());
        run("bash", &json!({"command":"cd sub"}), &ctx).await.unwrap();
        assert!(ctx.cwd().ends_with("sub"));
        assert_eq!(run("bash", &json!({"command":"pwd"}), &ctx).await.unwrap().trim(), ctx.cwd().display().to_string());
        // plain `&` must not hang the call
        let t = std::time::Instant::now();
        run("bash", &json!({"command":"sleep 30 & echo bg"}), &ctx).await.unwrap();
        assert!(t.elapsed().as_secs() < 3);
    }

    fn job_id(s: &str) -> String {
        s.split_whitespace().find(|w| w.len() > 3 && w.starts_with("job") && w[3..].chars().all(|c| c.is_ascii_digit())).unwrap().to_string()
    }

    #[tokio::test]
    async fn background_job_lifecycle() {
        let ctx = Ctx::new(tmp("job"));
        let started = run("bash", &json!({"command":"echo ready; sleep 60","background":true}), &ctx).await.unwrap();
        assert!(started.contains("job") && started.contains("ready"), "{started}");
        let id = job_id(&started);
        let o = run("bash_output", &json!({"id":id}), &ctx).await.unwrap();
        assert!(o.contains("running") && o.contains("ready"), "{o}");
        assert!(run("bash_output", &json!({}), &ctx).await.unwrap().contains(&id));
        run("bash_kill", &json!({"id":id}), &ctx).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        assert!(run("bash_kill", &json!({"id":"nope"}), &ctx).await.is_err());
        // short job finishes with an exit code
        let s2 = run("bash", &json!({"command":"echo hi; exit 7","background":true}), &ctx).await.unwrap();
        let id2 = job_id(&s2);
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        assert!(run("bash_output", &json!({"id":id2}), &ctx).await.unwrap().contains("exited with code 7"));
    }

    #[tokio::test]
    async fn big_output_is_spilled_to_a_file() {
        let ctx = Ctx::new(tmp("spill"));
        let o = run("bash", &json!({"command":"seq 1 20000"}), &ctx).await.unwrap();
        assert!(o.contains("truncated") && o.contains("full output saved to"), "{}", &o[o.len().saturating_sub(200)..]);
    }

    #[tokio::test]
    async fn multiedit_is_atomic() {
        let d = tmp("multi");
        std::fs::write(d.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        let ctx = Ctx::new(d.clone());
        run("read", &json!({"path":"f.txt"}), &ctx).await.unwrap();
        let bad = json!({"path":"f.txt","edits":[{"old_string":"one","new_string":"1"},{"old_string":"missing","new_string":"x"}]});
        assert!(run("multiedit", &bad, &ctx).await.is_err());
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "one\ntwo\nthree\n");
        let good = json!({"path":"f.txt","edits":[{"old_string":"one","new_string":"1"},{"old_string":"three","new_string":"3"}]});
        run("multiedit", &good, &ctx).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.join("f.txt")).unwrap(), "1\ntwo\n3\n");
    }
}
