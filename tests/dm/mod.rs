#![allow(dead_code)]
//! Helpers for daemon tests: spawn `zcode serve` with a clean environment and talk to it with the framed JSON protocol.
use crate::common::*;
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct Daemon {
    pub child: Child,
    pub sock: PathBuf,
    pub home: PathBuf,
}

impl Drop for Daemon {
    fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); }
}

/// The daemon gets NO credentials and no model config: everything must come from clients.
pub fn start(e: &Env, extra_cfg: Value, extra_env: &[(&str, &str)]) -> Daemon {
    let run = e.dir.join("runtime");
    let home = e.dir.join("dhome");
    std::fs::create_dir_all(&run).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let mut c = Command::new(env!("CARGO_BIN_EXE_zcode"));
    c.arg("serve").env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("HOME", &home)
        .env("XDG_RUNTIME_DIR", &run).env("XDG_DATA_HOME", home.join("data")).env("XDG_CACHE_HOME", home.join("cache")).env("XDG_STATE_HOME", home.join("state"))
        .env("ZCODE_CONFIG_CONTENT", extra_cfg.to_string()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
    for (k, v) in extra_env { c.env(k, v); }
    let child = c.spawn().unwrap();
    let sock = run.join("zcode/zcode.sock");
    let t = Instant::now();
    while UnixStream::connect(&sock).is_err() {
        assert!(t.elapsed() < Duration::from_secs(10), "daemon did not start");
        std::thread::sleep(Duration::from_millis(20));
    }
    Daemon { child, sock, home }
}

pub struct Client {
    s: UnixStream,
    n: u64,
    pub events: Vec<Value>,
}

fn read_frame(s: &mut UnixStream) -> Option<Value> {
    let mut len = [0u8; 4];
    s.read_exact(&mut len).ok()?;
    let mut buf = vec![0u8; u32::from_be_bytes(len) as usize];
    s.read_exact(&mut buf).ok()?;
    serde_json::from_slice(&buf).ok()
}

impl Client {
    pub fn connect(d: &Daemon) -> Client {
        let s = UnixStream::connect(&d.sock).unwrap();
        s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
        Client { s, n: 0, events: vec![] }
    }

    pub fn raw_send(&mut self, v: &Value) {
        let b = serde_json::to_vec(v).unwrap();
        self.s.write_all(&(b.len() as u32).to_be_bytes()).unwrap();
        self.s.write_all(&b).unwrap();
    }

    pub fn call(&mut self, op: &str, fields: Value) -> Value {
        self.n += 1;
        let mut v = json!({"req": self.n, "op": op});
        for (k, x) in fields.as_object().unwrap() { v[k] = x.clone(); }
        self.raw_send(&v);
        loop {
            let f = read_frame(&mut self.s).expect("daemon closed the connection");
            if f["req"] == self.n { return f; }
            self.events.push(f);
        }
    }

    pub fn ok(&mut self, op: &str, fields: Value) -> Value {
        let r = self.call(op, fields);
        assert_eq!(r["ok"], true, "{op} failed: {r}");
        r
    }

    /// Wait until an event matching `pred` arrives (already-buffered ones count).
    pub fn wait(&mut self, what: &str, pred: impl Fn(&Value) -> bool) -> Value {
        if let Some(i) = self.events.iter().position(&pred) { return self.events.remove(i); }
        let t = Instant::now();
        loop {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what}; got {:?}", self.events);
            let Some(f) = read_frame(&mut self.s) else { panic!("connection closed/timed out while waiting for {what}; got {:#?}", self.events.iter().map(|e| (e["type"].clone(), e["instance"].clone(), e["agent"].clone(), e["text"].clone(), e["part"]["state"]["output"].clone())).collect::<Vec<_>>()) };
            if f.get("req").is_some() { continue; }
            if pred(&f) { return f; }
            self.events.push(f);
        }
    }

    pub fn drain(&mut self, ms: u64) {
        self.s.set_read_timeout(Some(Duration::from_millis(ms))).unwrap();
        while let Some(f) = read_frame(&mut self.s) { if f.get("req").is_none() { self.events.push(f); } }
        self.s.set_read_timeout(Some(Duration::from_secs(20))).unwrap();
    }
}

pub fn client_config(llm: &FakeLlm, key: &str) -> Value {
    json!({"provider":{"fake":{"options":{"baseURL":llm.url,"apiKey":key},"models":{"m":{"limit":{"context":128000,"output":1000}},"s":{}}}},"model":"fake/m"})
}

pub fn create(c: &mut Client, name: &str, cwd: &std::path::Path, llm: &FakeLlm, extra: Value) -> Value {
    let mut f = json!({"instance": name, "cwd": cwd, "config": client_config(llm, "sk-client-secret"), "sandbox": "none"});
    for (k, v) in extra.as_object().unwrap() { f[k] = v.clone(); }
    c.ok("create", f)
}
