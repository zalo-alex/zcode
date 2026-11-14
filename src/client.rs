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
    /// image files for the first message, as base64
    pub images: Vec<String>,
}

fn provider(c: &crate::config::Config) -> Value {
    json!({"options": {"baseURL": c.base_url, "apiKey": c.api_key}, "models": {&c.model: {"limit": {"context": c.context, "output": c.max_output}, "vision": c.vision}}})
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
    macro_rules! next { () => {{ n += 1; n }} }
    if let (Ok(app), Ok(tok)) = (std::env::var("ZCODE_APP"), std::env::var("ZCODE_APP_TOKEN")) {
        call(&mut stream, json!({"req": next!(), "op": "hello", "app": app, "token": tok})).await.ok()?;
        let ack = read_frame(&mut stream).await.ok()?;
        if ack["ok"] != true { eprintln!("error: daemon rejected the app token: {}", ack["err"]); return Some(1); }
    }
    let create = json!({"op": "create", "instance": name, "cwd": r.cwd, "config": client_config(r.settings),
        "sandbox": r.sandbox, "require_sandbox": r.require_sandbox, "policy": r.policy, "max_workers": r.max_workers, "stream": r.format == Format::Default,
        "limits": {"max_time": r.max_time.unwrap_or(0), "max_steps": r.max_steps, "max_tokens": r.max_tokens}});
    let mut first = create.clone();
    first["req"] = json!(next!());
    call(&mut stream, first).await.ok()?;
    let ack = loop {
        let v = read_frame(&mut stream).await.ok()?;
        if v.get("req").is_some() { break v; }
    };
    if ack["ok"] != true {
        // the daemon refused (limit, bad request): behave exactly like the embedded runtime
        eprintln!("zcode: daemon refused the run ({}); running embedded", ack["err"].as_str().unwrap_or("?"));
        return None;
    }
    let prompt_fields = if r.images.is_empty() { json!({"text": r.prompt}) } else { json!({"text": r.prompt, "images": r.images.iter().map(|b| json!({"data": b})).collect::<Vec<_>>()}) };
    for (op, extra) in [("attach", json!({})), ("prompt", prompt_fields.clone())] {
        let mut v = json!({"req": next!(), "op": op, "instance": name});
        if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) { for (k, x) in e { o.insert(k.clone(), x.clone()); } }
        call(&mut stream, v).await.ok()?;
    }
    let human = r.format == Format::Default;
    let mut wrote_text = false;
    let code;
    // the daemon may restart under us (it restores its instances): events seen so far, and what to do after a reconnect
    let hello = match (std::env::var("ZCODE_APP"), std::env::var("ZCODE_APP_TOKEN")) { (Ok(a), Ok(t)) => Some((a, t)), _ => None };
    let (mut last_seq, mut replay, mut waiting_resume, mut resent) = (0u64, std::collections::VecDeque::<Value>::new(), false, false);
    let mut resend_req = None;
    loop {
        let v = if let Some(v) = replay.pop_front() { v } else {
            // after a reconnect, ten quiet seconds mean the restored instance did not take the turn back up (the daemon had not
            // saved it as in flight): send the prompt again, once. A turn that is running answers "busy", which is ignored.
            let wait = if waiting_resume { std::time::Duration::from_secs(10) } else { std::time::Duration::from_secs(365 * 86400) };
            match tokio::time::timeout(wait, read_frame(&mut stream)).await {
                Ok(Ok(v)) => v,
                Ok(Err(_)) => {
                    eprintln!("{}", if human { "daemon connection lost; waiting for it to come back...".yellow().to_string() } else { "daemon connection lost; waiting for it to come back...".to_string() });
                    match reconnect(&name, &create, &hello, last_seq, &mut n).await {
                        Some((s, events, running)) => { stream = s; waiting_resume = !running && events.is_empty(); replay.extend(events); resent = false; continue }
                        None => { eprintln!("error: lost connection to the daemon (the run could not be resumed)"); return Some(1) }
                    }
                }
                Err(_) => {
                    waiting_resume = false;
                    if !resent {
                        resent = true;
                        n += 1;
                        resend_req = Some(n);
                        let mut v = json!({"req": n, "op": "prompt", "instance": name});
                        if let Some(o) = v.as_object_mut() { for (k, x) in prompt_fields.as_object().into_iter().flatten() { o.insert(k.clone(), x.clone()); } }
                        if call(&mut stream, v).await.is_err() { continue; }
                    }
                    continue;
                }
            }
        };
        if let Some(s) = v["seq"].as_u64() { last_seq = last_seq.max(s); }
        if v.get("req").is_none() { waiting_resume = false; }
        if v.get("req").is_some() {
            if resend_req == Some(v["req"].as_u64().unwrap_or(0)) { continue; }
            if v["ok"] != true { eprintln!("error: {}", v["err"].as_str().unwrap_or("daemon error")); return Some(1); }
            continue;
        }
        let kind = v["type"].as_str().unwrap_or("");
        if !human {
            // identical to the embedded `--format json` stream, minus the daemon-only fields
            let mut o = v.clone();
            if let Some(m) = o.as_object_mut() { m.remove("instance"); m.remove("seq"); }
            // events only the daemon protocol has are not part of the embedded stream
            if kind != "turn_end" && !matches!(kind, "user" | "partial" | "tool_start") { println!("{o}"); let _ = std::io::stdout().flush(); }
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
    let _ = call(&mut stream, json!({"req": next!(), "op": "close", "instance": name})).await;
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), read_frame(&mut stream)).await;
    Some(code)
}

/// Wait for the daemon to come back (about a minute), take the instance back (`create` with `reattach`: a restored instance answers
/// `reattached`, anything else means it is gone) and attach after `since`. Returns the stream, the events that arrived on the way,
/// and whether a turn is running.
async fn reconnect(name: &str, create: &Value, hello: &Option<(String, String)>, since: u64, n: &mut u64) -> Option<(UnixStream, Vec<Value>, bool)> {
    async fn rpc(s: &mut UnixStream, n: &mut u64, mut v: Value, events: &mut Vec<Value>) -> Option<Value> {
        *n += 1;
        v["req"] = json!(*n);
        write_frame(s, &v).await.ok()?;
        loop {
            let r = tokio::time::timeout(std::time::Duration::from_secs(60), read_frame(s)).await.ok()?.ok()?;
            if r["req"] == *n { return Some(r); }
            if r.get("req").is_none() { events.push(r); }
        }
    }
    for _ in 0..60 {
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        let Ok(mut s) = UnixStream::connect(crate::serve::socket_path()).await else { continue };
        let mut events = vec![];
        if let Some((app, tok)) = hello {
            match rpc(&mut s, n, json!({"op": "hello", "app": app, "token": tok}), &mut events).await { Some(a) if a["ok"] == true => {} _ => continue }
        }
        let mut c = create.clone();
        c["reattach"] = json!(true);
        let Some(r) = rpc(&mut s, n, c, &mut events).await else { continue };
        if r["ok"] != true { continue }
        if r["reattached"] != true {
            // the daemon came back without our instance: undo the empty one `create` just made
            let _ = rpc(&mut s, n, json!({"op": "close", "instance": name}), &mut events).await;
            return None;
        }
        if !matches!(rpc(&mut s, n, json!({"op": "attach", "instance": name, "since": since}), &mut events).await, Some(a) if a["ok"] == true) { continue }
        let running = rpc(&mut s, n, json!({"op": "status", "instance": name}), &mut events).await
            .map(|st| st["instances"][0]["running"] == true || !st["instances"][0]["turn"].is_null()).unwrap_or(false);
        return Some((s, events, running));
    }
    None
}
