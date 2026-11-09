//! `zcode run` as a thin client of a running `zcode serve` daemon. Falls back to the embedded runtime (returns
//! `None`) whenever there is no daemon, so behavior does not depend on whether one is running.
use crate::config::Settings;
use crate::events::Format;
use crate::style::Paint;
use serde_json::{Value, json};
use std::io::Write;
use tokio::net::UnixStream;
use zcode_tools::frames::{read_frame, write_frame};

pub struct Req<'a> {
    pub settings: &'a Settings,
    pub cwd: &'a std::path::Path,
    pub sandbox: &'a str,
    pub require_sandbox: bool,
    pub policy: Value,
    pub max_time: Option<u64>,
    pub max_steps: usize,
    pub max_tokens: u64,
    pub max_workers: usize,
    pub format: Format,
    pub thinking: bool,
    pub prompt: &'a str,
}

fn provider(c: &crate::config::Config) -> Value {
    json!({"options": {"baseURL": c.base_url, "apiKey": c.api_key}, "models": {&c.model: {"limit": {"context": c.context, "output": c.max_output}}}})
}

/// The client's own resolved configuration (its env and files already applied): the daemon never uses its own.
pub fn client_config(s: &Settings) -> Value {
    let mut cfg = json!({"provider": {"main": provider(&s.main)}, "model": format!("main/{}", s.main.model)});
    if let Some(sm) = &s.small {
        cfg["provider"]["small"] = provider(sm);
        cfg["small_model"] = json!(format!("small/{}", sm.model));
    }
    for k in ["profiles", "swarm"] { if let Some(v) = s.raw.get(k) { cfg[k] = v.clone(); } }
    cfg
}

/// Some(exit code) when a daemon handled the run, None to run embedded.
pub async fn try_run(r: Req<'_>) -> Option<i32> {
    let mut stream = UnixStream::connect(crate::serve::socket_path()).await.ok()?;
    let name = format!("run-{}-{:x}", std::process::id(), std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0));
    async fn call(stream: &mut UnixStream, v: Value) -> std::io::Result<()> { write_frame(stream, &v).await }
    // optional per-app token from the client's own environment
    let mut n = 0u64;
    let mut next = || { n += 1; n };
    if let (Ok(app), Ok(tok)) = (std::env::var("ZCODE_APP"), std::env::var("ZCODE_APP_TOKEN")) {
        call(&mut stream, json!({"req": next(), "op": "hello", "app": app, "token": tok})).await.ok()?;
        let ack = read_frame(&mut stream).await.ok()?;
        if ack["ok"] != true { eprintln!("error: daemon rejected the app token: {}", ack["err"]); return Some(1); }
    }
    call(&mut stream, json!({"req": next(), "op": "create", "instance": name, "cwd": r.cwd, "config": client_config(r.settings),
        "sandbox": r.sandbox, "require_sandbox": r.require_sandbox, "policy": r.policy, "max_workers": r.max_workers, "stream": r.format == Format::Default,
        "limits": {"max_time": r.max_time.unwrap_or(0), "max_steps": r.max_steps, "max_tokens": r.max_tokens}})).await.ok()?;
    let ack = loop {
        let v = read_frame(&mut stream).await.ok()?;
        if v.get("req").is_some() { break v; }
    };
    if ack["ok"] != true {
        // the daemon refused (limit, bad request): behave exactly like the embedded runtime
        eprintln!("zcode: daemon refused the run ({}); running embedded", ack["err"].as_str().unwrap_or("?"));
        return None;
    }
    for (op, extra) in [("attach", json!({})), ("prompt", json!({"text": r.prompt}))] {
        let mut v = json!({"req": next(), "op": op, "instance": name});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) { for (k, x) in e { o.insert(k.clone(), x.clone()); } }
        call(&mut stream, v).await.ok()?;
    }
    let human = r.format == Format::Default;
    let mut wrote_text = false;
    let code;
    loop {
        let Ok(v) = read_frame(&mut stream).await else { eprintln!("error: lost connection to the daemon"); return Some(1) };
        if v.get("req").is_some() {
            if v["ok"] != true { eprintln!("error: {}", v["err"].as_str().unwrap_or("daemon error")); return Some(1); }
            continue;
        }
        let kind = v["type"].as_str().unwrap_or("");
        if !human {
            // identical to the embedded `--format json` stream, minus the daemon-only fields
            let mut o = v.clone();
            if let Some(m) = o.as_object_mut() { m.remove("instance"); m.remove("seq"); }
            if kind != "turn_end" { println!("{o}"); let _ = std::io::stdout().flush(); }
        } else {
            match kind {
                "text_delta" => { print!("{}", v["delta"].as_str().unwrap_or("")); let _ = std::io::stdout().flush(); wrote_text = true; }
                "reasoning_delta" if r.thinking => { eprint!("{}", v["delta"].as_str().unwrap_or("").dimmed().italic()); }
                "tool_start" => eprintln!("{} {}", "▸".cyan(), v["summary"].as_str().unwrap_or("").dimmed()),
                "step_finish" => { if wrote_text { println!(); wrote_text = false; } }
                "warning" => eprintln!("{}", v["message"].as_str().unwrap_or("")),
                "error" if v["error"]["type"] == "limit" => eprintln!("{}", format!("[stopped: {}]", v["error"]["message"].as_str().unwrap_or("")).yellow()),
                "error" if v["error"]["type"] == "interrupted" => println!("\n{}", "^C interrupted".yellow()),
                _ => {}
            }
        }
        if kind == "turn_end" {
            code = v["code"].as_i64().unwrap_or(1) as i32;
            if human && v["stop"] == "error" { eprintln!("error: {}", v["message"].as_str().unwrap_or("")); }
            break;
        }
    }
    let _ = call(&mut stream, json!({"req": next(), "op": "close", "instance": name})).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut stream)).await;
    Some(code)
}
