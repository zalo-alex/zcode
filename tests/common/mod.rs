#![allow(dead_code)]
//! Fake OpenAI-compatible server (SSE) + helpers to run the zcode binary against it.
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Clone, Debug)]
pub enum Script {
    Text(String),
    /// (tool name, arguments JSON)
    Tools(Vec<(&'static str, String)>),
    /// text finished by finish_reason=length
    Length(String),
    /// (reasoning, answer)
    Reasoning(String, String),
    Status(u16, String),
    Empty,
    /// send headers + first byte, then hang for N seconds
    Stall(u64),
}

pub struct FakeLlm {
    pub url: String,
    pub requests: Arc<Mutex<Vec<Value>>>,
}

fn sse(chunks: &[Value]) -> String {
    let mut s = String::new();
    for c in chunks { s.push_str(&format!("data: {c}\n\n")); }
    s.push_str("data: [DONE]\n\n");
    s
}

fn respond(mut sock: TcpStream, script: Script) {
    let head = "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: close\r\n\r\n";
    let role = json!({"choices":[{"index":0,"delta":{"role":"assistant","content":""}}]});
    let usage = |finish: &str| json!({"choices":[{"index":0,"delta":{},"finish_reason":finish}],"usage":{"prompt_tokens":100,"completion_tokens":10}});
    let body = match script {
        Script::Text(t) => sse(&[role, json!({"choices":[{"delta":{"content":t}}]}), usage("stop")]),
        Script::Reasoning(r, t) => sse(&[role, json!({"choices":[{"delta":{"reasoning_content":r}}]}), json!({"choices":[{"delta":{"content":t}}]}), usage("stop")]),
        Script::Length(t) => sse(&[role, json!({"choices":[{"delta":{"content":t}}]}), usage("length")]),
        Script::Empty => sse(&[role, usage("stop")]),
        Script::Tools(calls) => {
            let mut chunks = vec![role];
            for (i, (name, args)) in calls.iter().enumerate() {
                let half = args.len() / 2;
                chunks.push(json!({"choices":[{"delta":{"tool_calls":[{"index":i,"id":format!("call_{i}_{}", name),"type":"function","function":{"name":name,"arguments":&args[..half]}}]}}]}));
                chunks.push(json!({"choices":[{"delta":{"tool_calls":[{"index":i,"function":{"arguments":&args[half..]}}]}}]}));
            }
            chunks.push(usage("tool_calls"));
            sse(&chunks)
        }
        Script::Status(code, b) => {
            let _ = write!(sock, "HTTP/1.1 {code} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{b}", b.len());
            return;
        }
        Script::Stall(secs) => {
            let _ = sock.write_all(head.as_bytes());
            let _ = sock.write_all(format!("data: {role}\n\n").as_bytes());
            let _ = sock.flush();
            std::thread::sleep(Duration::from_secs(secs));
            return;
        }
    };
    let _ = sock.write_all(head.as_bytes());
    let _ = sock.write_all(body.as_bytes());
    let _ = sock.flush();
}

pub type Router = Arc<dyn Fn(&Value) -> Script + Send + Sync>;

fn handle(mut sock: TcpStream, reqs: Arc<Mutex<Vec<Value>>>, queue: Arc<Mutex<VecDeque<Script>>>, default: Option<Script>, router: Option<Router>) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 8192];
    let header_end = loop {
        let n = sock.read(&mut tmp).unwrap_or(0);
        if n == 0 { return; }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") { break i + 4; }
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).to_lowercase();
    let len: usize = head.lines().find_map(|l| l.strip_prefix("content-length:")).and_then(|v| v.trim().parse().ok()).unwrap_or(0);
    while buf.len() < header_end + len {
        let n = sock.read(&mut tmp).unwrap_or(0);
        if n == 0 { break; }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body: Value = serde_json::from_slice(&buf[header_end..]).unwrap_or(Value::Null);
    let script = match &router {
        Some(f) => f(&body),
        None => queue.lock().unwrap().pop_front().or(default).unwrap_or(Script::Text("(no script left)".into())),
    };
    reqs.lock().unwrap().push(body);
    respond(sock, script);
}

impl FakeLlm {
    pub fn start(scripts: Vec<Script>) -> FakeLlm { Self::start_with_default(scripts, None) }

    /// After `scripts` run out, `default` is repeated forever.
    /// Route by request content: lets several concurrent agents get deterministic replies.
    pub fn start_fn(f: impl Fn(&Value) -> Script + Send + Sync + 'static) -> FakeLlm {
        Self::start_inner(vec![], None, Some(Arc::new(f)))
    }

    pub fn start_with_default(scripts: Vec<Script>, default: Option<Script>) -> FakeLlm { Self::start_inner(scripts, default, None) }

    fn start_inner(scripts: Vec<Script>, default: Option<Script>, router: Option<Router>) -> FakeLlm {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://127.0.0.1:{}/v1", l.local_addr().unwrap().port());
        let reqs = Arc::new(Mutex::new(vec![]));
        let queue = Arc::new(Mutex::new(VecDeque::from(scripts)));
        let (r, q) = (reqs.clone(), queue.clone());
        std::thread::spawn(move || {
            for s in l.incoming().flatten() {
                let (r, q, d, f) = (r.clone(), q.clone(), default.clone(), router.clone());
                std::thread::spawn(move || handle(s, r, q, d, f));
            }
        });
        FakeLlm { url, requests: reqs }
    }

    pub fn count(&self) -> usize { self.requests.lock().unwrap().len() }
    pub fn req(&self, i: usize) -> Value { self.requests.lock().unwrap()[i].clone() }
}

pub struct Env {
    pub dir: PathBuf,
    pub home: PathBuf,
    pub cwd: PathBuf,
}

impl Env {
    pub fn new(name: &str) -> Env {
        let dir = std::env::temp_dir().join(format!("zcode_it_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (home, cwd) = (dir.join("home"), dir.join("work"));
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&cwd).unwrap();
        Env { dir, home, cwd: cwd.canonicalize().unwrap() }
    }

    pub fn session_dir(&self) -> PathBuf { self.home.join("data/zcode/sessions") }

    pub fn sessions(&self) -> Vec<PathBuf> {
        std::fs::read_dir(self.session_dir()).map(|d| d.flatten().map(|e| e.path()).collect()).unwrap_or_default()
    }

    pub fn config(&self, llm: &FakeLlm, extra: Value) -> String {
        let mut c = json!({"provider":{"fake":{"options":{"baseURL":llm.url,"apiKey":"k"},
            "models":{"m":{"limit":{"context":128000,"output":1000}},"s":{}}}},"model":"fake/m"});
        zcode_merge(&mut c, extra);
        c.to_string()
    }

    pub fn cmd(&self, llm: &FakeLlm, args: &[&str]) -> Command { self.cmd_cfg(llm, json!({}), args) }

    pub fn cmd_cfg(&self, llm: &FakeLlm, extra: Value, args: &[&str]) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_zcode"));
        c.args(args).current_dir(&self.cwd).stdin(std::process::Stdio::null())
            .env("HOME", &self.home).env("XDG_DATA_HOME", self.home.join("data"))
            .env("XDG_CACHE_HOME", self.home.join("cache")).env("XDG_CONFIG_HOME", self.home.join("config"))
            .env("OPENCODE_CONFIG_CONTENT", self.config(llm, extra))
            .env_remove("ZCODE_BASE_URL").env_remove("ZCODE_API_KEY").env_remove("ZCODE_MODEL").env_remove("OPENCODE_MODEL")
            .env_remove("ZCODE_CONFIG_CONTENT").env_remove("NO_COLOR");
        c
    }

    pub fn run(&self, llm: &FakeLlm, args: &[&str]) -> Output { self.cmd(llm, args).output().unwrap() }
}

fn zcode_merge(a: &mut Value, b: Value) {
    match (a, b) {
        (Value::Object(a), Value::Object(b)) => for (k, v) in b { zcode_merge(a.entry(k).or_insert(Value::Null), v); },
        (a, b) => *a = b,
    }
}

pub fn out(o: &Output) -> String { String::from_utf8_lossy(&o.stdout).into_owned() }
pub fn err(o: &Output) -> String { String::from_utf8_lossy(&o.stderr).into_owned() }
pub fn code(o: &Output) -> i32 { o.status.code().unwrap_or(-1) }

pub fn json_lines(o: &Output) -> Vec<Value> {
    out(o).lines().filter_map(|l| serde_json::from_str(l).ok()).collect()
}

/// Write a session file with `n` user/assistant pairs for `cwd`.
pub fn seed_session(env: &Env, id: &str, pairs: usize) -> PathBuf {
    std::fs::create_dir_all(env.session_dir()).unwrap();
    let mut s = format!("{}\n", json!({"zcode_session":{"id":id,"cwd":env.cwd.display().to_string(),"model":"m","created":0}}));
    for i in 0..pairs {
        s.push_str(&format!("{}\n{}\n", json!({"role":"user","content":format!("question {i}")}), json!({"role":"assistant","content":format!("answer {i}")})));
    }
    let p = env.session_dir().join(format!("{id}.jsonl"));
    std::fs::write(&p, s).unwrap();
    p
}

pub fn path_str(p: &Path) -> String { p.display().to_string() }
