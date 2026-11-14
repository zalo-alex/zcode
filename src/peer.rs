//! `zcode --group NAME --auto [--agent NAME]`: the interactive interface for an agent that lives in the daemon, in a
//! group with other agents. Everything the agent does is shown here as it happens, including what it does when another
//! agent's message wakes it; your own lines are queued while it is busy. The agents message each other through the
//! group's in-memory bus inside the daemon; this process only carries your keystrokes and renders events.
//!
//! `zcode --attach INSTANCE` is the same interface for an instance that something else already created (a program driving the
//! daemon): it only attaches, and leaving does not close the instance.
use crate::client::client_config;
use crate::config::Settings;
use crate::style::Paint;
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, IsTerminal};
use std::time::Duration;
use tokio::net::UnixStream;
use tokio::net::unix::OwnedWriteHalf;
use tokio::sync::mpsc;
use zcode_tools::frames::{read_frame, write_frame};

pub struct Join<'a> {
    pub settings: &'a Settings,
    pub cwd: &'a std::path::Path,
    pub group: String,
    pub name: String,
    pub sandbox: &'a str,
    pub require_sandbox: bool,
    pub policy: Value,
    pub max_workers: usize,
    pub max_steps: usize,
    pub max_tokens: u64,
    pub max_time: Option<u64>,
    pub session: Option<String>,
}

const HELP: &str = "/help  /agents  /cost  /status  /todo  /clear  /compact  /image <path>  /thinking [on|off]  /exit   (Ctrl-C interrupts your agent; anything else you type is sent to it)";

/// A line to send: its text and the images (base64) attached to it.
type Line = (String, Vec<String>);

enum Input { Line(String), Interrupt, Eof }

/// Prints without tearing the line being typed (rustyline's external printer on a terminal, stdout otherwise).
struct Out { printer: Option<Box<dyn rustyline::ExternalPrinter + Send>> }
impl Out {
    fn line(&mut self, s: &str) {
        match &mut self.printer {
            Some(p) => { let _ = p.print(format!("{s}\n")); }
            None => println!("{s}"),
        }
    }
}

/// Requests made before the event loop starts: replies and events share the socket, so skip events while waiting.
async fn call(s: &mut UnixStream, n: &mut u64, op: &str, fields: Value) -> Result<Value, String> {
    *n += 1;
    let mut v = json!({"req": *n, "op": op});
    if let (Some(o), Some(f)) = (v.as_object_mut(), fields.as_object()) { for (k, x) in f { o.insert(k.clone(), x.clone()); } }
    write_frame(s, &v).await.map_err(|e| e.to_string())?;
    loop {
        let r = tokio::time::timeout(Duration::from_secs(120), read_frame(s)).await.map_err(|_| "the daemon did not answer".to_string())?.map_err(|_| "lost connection to the daemon".to_string())?;
        if r["req"] == *n {
            return if r["ok"] == true { Ok(r) } else { Err(r["err"].as_str().unwrap_or("daemon error").to_string()) };
        }
    }
}

fn names(agents: &Value) -> Vec<String> {
    agents.as_array().into_iter().flatten().filter_map(|l| l.as_str()?.split_whitespace().next().map(String::from)).filter(|n| n != "swarm").collect()
}

async fn connect(n: &mut u64) -> anyhow::Result<UnixStream> {
    let path = crate::serve::socket_path();
    let mut s = UnixStream::connect(&path).await.map_err(|_| anyhow::anyhow!("no zcode daemon is running at {}; start one with `zcode serve`", path.display()))?;
    if let (Ok(app), Ok(tok)) = (std::env::var("ZCODE_APP"), std::env::var("ZCODE_APP_TOKEN")) {
        call(&mut s, n, "hello", json!({"app": app, "token": tok})).await.map_err(|e| anyhow::anyhow!("daemon rejected the app token: {e}"))?;
    }
    Ok(s)
}

/// Attach to an instance that already exists (created by another client) and use it interactively. Leaving detaches only.
pub async fn attach(name: String) -> anyhow::Result<i32> {
    let mut n = 0u64;
    let mut s = connect(&mut n).await?;
    let st = call(&mut s, &mut n, "status", json!({"instance": name})).await.map_err(|e| anyhow::anyhow!("{e}"))?;
    let i = &st["instances"][0];
    let built = call(&mut s, &mut n, "build", json!({"instance": name})).await.map_err(anyhow::Error::msg)?;
    // start from now: an instance that has been running for days must not replay its whole event ring into the terminal
    call(&mut s, &mut n, "attach", json!({"instance": name, "since": i["events"].as_u64().unwrap_or(0)})).await.map_err(anyhow::Error::msg)?;
    let group = i["group"].as_str().unwrap_or("").to_string();
    let banner = vec![
        format!("{} {} · {}", "zcode".bold().cyan(), env!("CARGO_PKG_VERSION"), i["cwd"].as_str().unwrap_or("")),
        format!("attached to `{name}`{} · auto-approve · leaving detaches, the agent keeps running", if group.is_empty() { String::new() } else { format!(" (group `{group}`)") }).dimmed().to_string(),
        format!("in the group: {}", names(&built["agents"]).join(", ")).dimmed().to_string(),
        HELP.dimmed().to_string(),
    ];
    interact(s, n, name, group, banner, i["thinking"].as_bool(), None).await
}

pub async fn run(j: Join<'_>) -> anyhow::Result<i32> {
    let mut n = 0u64;
    let mut s = connect(&mut n).await?;
    let mut create = json!({"instance": j.name, "cwd": j.cwd, "config": client_config(j.settings), "sandbox": j.sandbox, "require_sandbox": j.require_sandbox,
        "policy": j.policy, "group": j.group, "max_workers": j.max_workers, "stream": true, "close_on_disconnect": true,
        "limits": {"max_time": j.max_time.unwrap_or(0), "max_steps": j.max_steps, "max_tokens": j.max_tokens}});
    if let Some(sid) = &j.session { create["session"] = json!(sid); }
    let made = call(&mut s, &mut n, "create", create.clone()).await.map_err(|e| {
        if e.contains("already exists") { anyhow::anyhow!("{e}; pick another name with --agent") } else { anyhow::anyhow!("the daemon refused: {e}") }
    })?;
    // after a daemon restart the same request (same session) brings the agent back
    create["session"] = made["session"].clone();
    let built = match call(&mut s, &mut n, "build", json!({"instance": j.name})).await {
        Ok(b) => b,
        Err(e) => { let _ = call(&mut s, &mut n, "close", json!({"instance": j.name})).await; anyhow::bail!("{e}"); }
    };
    call(&mut s, &mut n, "attach", json!({"instance": j.name})).await.map_err(anyhow::Error::msg)?;
    let banner = vec![
        format!("{} {} · {} · {}", "zcode".bold().cyan(), env!("CARGO_PKG_VERSION"), j.settings.main.model, j.cwd.display()),
        format!("joined group `{}` as `{}` · auto-approve · sandbox {}", j.group, j.name, j.sandbox).dimmed().to_string(),
        format!("in the group: {}", names(&built["agents"]).join(", ")).dimmed().to_string(),
        "your agent can message the others with send; what they send back shows up here as it arrives".dimmed().to_string(),
        HELP.dimmed().to_string(),
    ];
    interact(s, n, j.name, j.group, banner, None, Some(create)).await
}

/// The prompt loop, shared by `run` and `attach`. `recreate` (only for the agent we created) is its `create` request: it ends the
/// instance on exit and brings it back after a daemon restart; without it the terminal just detaches.
async fn interact(s: UnixStream, n: u64, name: String, group: String, banner: Vec<String>, thinking: Option<bool>, recreate: Option<Value>) -> anyhow::Result<i32> {
    // an agent we created (recreate request known) is closed on exit; an attached one only detaches
    let close = recreate.is_some();
    // keyboard: a thread (readline blocks), or plain lines when stdin is not a terminal
    let tty = std::io::stdin().is_terminal();
    let (in_tx, mut in_rx) = mpsc::unbounded_channel::<Input>();
    let printer: Option<Box<dyn rustyline::ExternalPrinter + Send>> = if tty {
        let (pt, pr) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let Ok(mut rl) = rustyline::DefaultEditor::new() else { let _ = in_tx.send(Input::Eof); return };
            let Ok(p) = rl.create_external_printer() else { let _ = in_tx.send(Input::Eof); return };
            let _ = pt.send(Box::new(p) as Box<dyn rustyline::ExternalPrinter + Send>);
            loop {
                match rl.readline("› ") {
                    Ok(l) => { let _ = rl.add_history_entry(l.as_str()); if in_tx.send(Input::Line(l)).is_err() { break; } }
                    Err(rustyline::error::ReadlineError::Interrupted) => { if in_tx.send(Input::Interrupt).is_err() { break; } }
                    Err(_) => { let _ = in_tx.send(Input::Eof); break; }
                }
            }
        });
        pr.recv().ok()
    } else {
        std::thread::spawn(move || {
            for l in std::io::stdin().lock().lines() {
                let Ok(l) = l else { break };
                if in_tx.send(Input::Line(l)).is_err() { return; }
            }
            let _ = in_tx.send(Input::Eof);
        });
        None
    };
    let mut out = Out { printer };

    for l in &banner { println!("{l}"); }

    let (rd, wr) = s.into_split();
    let (fr_tx, mut fr_rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(read_loop(rd, fr_tx));

    let mut p = Peer { name: name.clone(), group: group.clone(), wr, n, busy: false, pending: VecDeque::new(), prompts: HashMap::new(), agents_req: None, buf: String::new(), tin: 0, tout: 0, eof: false, streamed: false, rbuf: String::new(), thinking, thinking_req: None, seq: 0, cmd_reqs: HashMap::new(), images: vec![], cwd: std::env::current_dir().unwrap_or_default() };
    let mut code = 0;
    loop {
        tokio::select! {
            inp = in_rx.recv() => match inp {
                Some(Input::Line(l)) => { if p.line(&mut out, l.trim()).await { break; } }
                Some(Input::Interrupt) => {
                    if p.busy { p.send("cancel", json!({})).await; out.line(&"^C interrupted".yellow()); }
                    else { out.line(&"(type /exit to leave the group)".dimmed()); }
                }
                Some(Input::Eof) | None => { p.eof = true; }
            },
            fr = fr_rx.recv() => match fr {
                Some(v) => p.frame(&mut out, v).await,
                None => {
                    out.line(&"daemon connection lost; waiting for it to come back...".yellow());
                    match reconnect(&name, &recreate, p.seq, &mut p.n).await {
                        Some(s) => {
                            let (rd, wr) = s.into_split();
                            let (tx, rx) = mpsc::unbounded_channel::<Value>();
                            tokio::spawn(read_loop(rd, tx));
                            (p.wr, fr_rx) = (wr, rx);
                            // prompts the old connection never answered go out again; the restored agent may also be resuming by itself
                            let mut lost: Vec<Line> = p.prompts.drain().map(|(_, v)| v).collect();
                            lost.reverse();
                            for l in lost { p.pending.push_front(l); }
                            p.busy = false;
                            out.line(&"reconnected".green());
                        }
                        None => { out.line(&"error: lost connection to the daemon".red()); code = 1; break; }
                    }
                }
            },
        }
        // end of piped input: leave once the agent has nothing left to do
        if p.eof && !p.busy && p.pending.is_empty() { break; }
    }
    // leave: a group agent we created is closed by the daemon (its session stays on disk); an attached one only detaches
    p.send(if close { "close" } else { "detach" }, json!({})).await;
    let _ = tokio::time::timeout(Duration::from_secs(3), async { while let Some(v) = fr_rx.recv().await { if v.get("req").is_some() && v["req"] == p.n { break; } } }).await;
    Ok(code)
}

async fn read_loop(mut rd: tokio::net::unix::OwnedReadHalf, tx: mpsc::UnboundedSender<Value>) {
    while let Ok(v) = read_frame(&mut rd).await { if tx.send(v).is_err() { break; } }
}

/// Wait for the daemon to come back (it restores its instances from disk), then take the instance back and
/// replay the events after `since`. Gives up after about a minute.
async fn reconnect(name: &str, recreate: &Option<Value>, since: u64, n: &mut u64) -> Option<UnixStream> {
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_secs(1)).await;
        let Ok(mut s) = connect(n).await else { continue };
        if let Some(c) = recreate {
            let mut c = c.clone();
            c["reattach"] = json!(true);
            if call(&mut s, n, "create", c).await.is_err() { continue; }
        }
        if call(&mut s, n, "build", json!({"instance": name})).await.is_err() { continue; }
        if call(&mut s, n, "attach", json!({"instance": name, "since": since})).await.is_err() { continue; }
        return Some(s);
    }
    None
}

struct Peer {
    /// last event counter seen, to resume the stream after a reconnect
    seq: u64,
    name: String,
    group: String,
    wr: OwnedWriteHalf,
    n: u64,
    busy: bool,
    pending: VecDeque<Line>,
    /// request id of an in-flight `prompt` -> its text, to requeue it when the agent turned out to be busy
    prompts: HashMap<u64, Line>,
    agents_req: Option<u64>,
    /// the model's text so far, up to its last newline
    buf: String,
    tin: u64,
    tout: u64,
    eof: bool,
    /// this step already streamed its text (`text` repeats it at the end of the step)
    streamed: bool,
    /// reasoning streamed so far, up to its last newline (shown dim and italic)
    rbuf: String,
    thinking: Option<bool>,
    thinking_req: Option<u64>,
    /// `/clear`, `/compact`, `/todo` in flight: request id -> command
    cmd_reqs: HashMap<u64, &'static str>,
    /// images from `/image`, sent with the next line
    images: Vec<String>,
    cwd: std::path::PathBuf,
}

impl Peer {
    async fn send(&mut self, op: &str, fields: Value) -> u64 {
        self.n += 1;
        let mut v = json!({"req": self.n, "op": op, "instance": self.name});
        if let (Some(o), Some(f)) = (v.as_object_mut(), fields.as_object()) { for (k, x) in f { o.insert(k.clone(), x.clone()); } }
        let _ = write_frame(&mut self.wr, &v).await;
        self.n
    }

    async fn prompt(&mut self, line: Line) {
        let fields = if line.1.is_empty() { json!({"text": line.0}) } else { json!({"text": line.0, "images": line.1.iter().map(|b| json!({"data": b})).collect::<Vec<_>>()}) };
        let id = self.send("prompt", fields).await;
        self.prompts.insert(id, line);
        self.busy = true;
    }

    fn flush_reasoning(&mut self, out: &mut Out) {
        if !self.rbuf.is_empty() { let b = std::mem::take(&mut self.rbuf); out.line(&b.dimmed().italic().to_string()); }
    }

    fn flush(&mut self, out: &mut Out) {
        self.flush_reasoning(out);
        if !self.buf.is_empty() { let b = std::mem::take(&mut self.buf); out.line(&b); }
    }

    /// One line from the keyboard. true = leave.
    async fn line(&mut self, out: &mut Out, l: &str) -> bool {
        if l.is_empty() { return false; }
        if let Some(cmd) = l.strip_prefix('/') {
            match cmd.split_whitespace().next().unwrap_or("") {
                "exit" | "quit" | "q" => return true,
                "help" => out.line(HELP),
                "agents" => { let id = self.send("status", json!({})).await; self.agents_req = Some(id); }
                "cost" => out.line(&format!("tokens in {} / out {}", self.tin, self.tout)),
                "status" => out.line(&format!("group {} · agent {} · {} · thinking {}", self.group, self.name, if self.busy { "working" } else { "idle" }, match self.thinking { Some(true) => "on", Some(false) => "off", None => "?" })),
                c @ ("clear" | "compact" | "todo") => {
                    let c: &'static str = match c { "clear" => "clear", "compact" => "compact", _ => "todo" };
                    if self.busy && c != "todo" { out.line("your agent is busy; try again when it is done"); return false; }
                    let id = self.send(c, json!({})).await;
                    self.cmd_reqs.insert(id, c);
                }
                "image" => {
                    let arg = cmd["image".len()..].trim().trim_matches(|c| c == '"' || c == '\'');
                    if arg.is_empty() { out.line("usage: /image <path>   (attaches to your next message)"); return false; }
                    let p = std::path::Path::new(arg);
                    let p = if p.is_absolute() { p.to_path_buf() } else { self.cwd.join(p) };
                    match std::fs::read(&p) {
                        Err(e) => out.line(&format!("error: {}: {e}", p.display())),
                        Ok(b) if b.len() > zcode_tools::img::MAX_SRC => out.line(&format!("error: {}: {}", p.display(), zcode_tools::img::too_big_note(b.len()))),
                        Ok(b) if zcode_tools::img::sniff(&b).is_none() => out.line(&format!("error: {}: not a supported image (jpeg, png, gif, webp)", p.display())),
                        Ok(b) => { self.images.push(zcode_tools::img::b64_encode(&b)); out.line(&format!("attached {}", p.display()).dimmed()); }
                    }
                }
                "thinking" => {
                    let arg = cmd.split_whitespace().nth(1).unwrap_or("");
                    let on = match arg { "on" => Some(true), "off" => Some(false), "" => None, _ => { out.line("usage: /thinking [on|off]"); return false; } };
                    let id = self.send("thinking", match on { Some(b) => json!({"on": b}), None => json!({}) }).await;
                    self.thinking_req = Some(id);
                }
                _ => out.line(&format!("unknown command; {HELP}")),
            }
            return false;
        }
        if self.busy {
            self.pending.push_back((l.to_string(), std::mem::take(&mut self.images)));
            out.line(&"(your agent is busy; sent when it is done)".dimmed());
        } else {
            let images = std::mem::take(&mut self.images);
            self.prompt((l.to_string(), images)).await;
        }
        false
    }

    /// A reply or an event from the daemon.
    async fn frame(&mut self, out: &mut Out, v: Value) {
        if let Some(s) = v["seq"].as_u64() { self.seq = self.seq.max(s); }
        if let Some(id) = v["req"].as_u64() {
            if let Some(line) = self.prompts.remove(&id) {
                if v["ok"] != true {
                    let e = v["err"].as_str().unwrap_or("");
                    if e.contains("busy") { self.pending.push_front(line); self.busy = true; }  // a peer's message woke it first: our turn_end will resend
                    else { out.line(&format!("{} {e}", "error:".red())); self.busy = false; }
                }
            } else if self.thinking_req == Some(id) {
                self.thinking_req = None;
                if v["ok"] == true {
                    let on = v["thinking"].as_bool().unwrap_or(false);
                    self.thinking = Some(on);
                    out.line(&format!("thinking {} (from your agent's next turn)", if on { "on" } else { "off" }));
                } else { out.line(&format!("{} {}", "error:".red(), v["err"].as_str().unwrap_or(""))); }
            } else if let Some(cmd) = self.cmd_reqs.remove(&id) {
                if v["ok"] != true { out.line(&format!("{} {}", "error:".red(), v["err"].as_str().unwrap_or(""))); }
                else { match cmd {
                    "clear" => out.line("cleared"),
                    "compact" => out.line(&format!("compacted → ~{} tokens", v["tokens"])),
                    _ => {
                        let todos = v["todos"].as_array().cloned().unwrap_or_default();
                        if todos.is_empty() { out.line("(no todos)"); }
                        for t in &todos { out.line(&format!("[{}] {}", match t["status"].as_str().unwrap_or("") { "completed" => "x", "in_progress" => "~", _ => " " }, t["content"].as_str().unwrap_or(""))); }
                    }
                } }
            } else if self.agents_req == Some(id) {
                self.agents_req = None;
                if let Some(l) = v["instances"][0]["agents"].as_array() { for a in l.iter().filter_map(|x| x.as_str()) { out.line(a); } }
            }
            return;
        }
        let mine = v["agent"].as_str() == Some(self.name.as_str());
        match v["type"].as_str().unwrap_or("") {
            "step_start" if mine => { self.busy = true; self.streamed = false; }
            // an instance created without `stream`: the whole text of a step arrives at once
            "text" if mine && !self.streamed => { for l in v["part"]["text"].as_str().unwrap_or("").lines() { out.line(l); } }
            "reasoning_delta" if mine => {
                self.rbuf.push_str(v["delta"].as_str().unwrap_or(""));
                while let Some(i) = self.rbuf.find('\n') { let line: String = self.rbuf.drain(..=i).collect(); out.line(&line.trim_end_matches('\n').dimmed().italic().to_string()); }
            }
            "text_delta" if mine => {
                self.flush_reasoning(out);
                self.streamed = true;
                self.buf.push_str(v["delta"].as_str().unwrap_or(""));
                while let Some(i) = self.buf.find('\n') { let line: String = self.buf.drain(..=i).collect(); out.line(line.trim_end_matches('\n')); }
            }
            "tool_start" if mine => { self.flush(out); out.line(&format!("{} {}", "▸".cyan(), v["summary"].as_str().unwrap_or("").dimmed())); }
            "step_finish" if mine => {
                self.flush(out);
                self.tin += v["part"]["tokens"]["input"].as_u64().unwrap_or(0);
                self.tout += v["part"]["tokens"]["output"].as_u64().unwrap_or(0);
            }
            // a message from another agent, delivered to this one
            "incoming" => {
                self.flush(out);
                let text = v["text"].as_str().unwrap_or("");
                let mut lines = text.lines();
                out.line(&format!("{} {}", format!("‹ {}", v["from"].as_str().unwrap_or("?")).bold().cyan(), lines.next().unwrap_or("")));
                for l in lines.take(7) { out.line(&format!("  {l}")); }
            }
            "spawn" if mine => out.line(&format!("· started worker {} (tier {})", v["agent"].as_str().unwrap_or("?"), v["tier"]).dimmed()),
            "warning" => out.line(&v["message"].as_str().unwrap_or("").yellow()),
            "error" if mine && v["error"]["type"] == "limit" => out.line(&format!("[stopped: {}]", v["error"]["message"].as_str().unwrap_or("")).yellow()),
            "error" if mine && v["error"]["type"] == "interrupted" => {}
            "turn_end" if mine => {
                self.flush(out);
                self.busy = false;
                if v["stop"] == "error" { out.line(&format!("{} {}", "error:".red(), v["message"].as_str().unwrap_or(""))); }
                if let Some(next) = self.pending.pop_front() { self.prompt(next).await; }
            }
            _ => {}
        }
    }
}
