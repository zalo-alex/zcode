//! `zcode serve`: one resident process hosting many agent instances on the same in-memory bus. The socket only
//! carries control requests and event streams (length-prefixed JSON, multiplexed by instance id); agent-to-agent
//! messages are channels, never the socket. cwd, model, key, policy and limits come from each client's `create`
//! request, never from the daemon's environment, and the API key never leaves the daemon.
use crate::agent::{Agent, AgentOpts, Cancel, Limits, Stop};
use crate::events;
use crate::guard::GuardMode;
use crate::llm::Client;
use crate::session::{self, Session};
use crate::swarm::{Budget, SpawnCtx, Swarm, SwarmCfg};
use crate::{config, context};
use serde_json::{Value, json};
use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::{Semaphore, broadcast, mpsc};
use zcode_tools::frames::{read_frame, write_frame};
use zcode_tools::{self as tools, Mode, Policy, Sandbox};

pub fn socket_path() -> PathBuf {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() && std::path::Path::new(&d).is_dir() => PathBuf::from(d).join("zcode/zcode.sock"),
        _ => PathBuf::from(format!("/tmp/zcode-{}/zcode.sock", unsafe { libc::getuid() })),
    }
}

struct ServeCfg {
    max_instances: usize,
    max_history_bytes: usize,
    ring: usize,
    /// app -> token; empty = no authentication beyond SO_PEERCRED
    apps: HashMap<String, String>,
}

struct Daemon {
    cfg: ServeCfg,
    instances: Mutex<HashMap<String, Arc<Instance>>>,
    groups: Mutex<HashMap<String, Arc<Swarm>>>,
    clients: Mutex<HashMap<String, Arc<Client>>>,
    gate: Arc<Semaphore>,
    global: Arc<Budget>,
}

struct Params {
    cwd: PathBuf,
    config: Value,
    model: Option<String>,
    mode: Mode,
    require_sandbox: bool,
    policy: Value,
    limits: Limits,
    group: Option<String>,
    max_workers: usize,
    stream: bool,
    session: String,
    swarm: Value,
}

#[derive(Default)]
struct EventBuf {
    ring: VecDeque<Value>,
    seq: u64,
}

struct Instance {
    name: String,
    app: String,
    p: Params,
    agent: Mutex<Option<Agent>>,
    build: tokio::sync::Mutex<bool>,
    running: AtomicBool,
    crashed: AtomicBool,
    swarm: Mutex<Option<Arc<Swarm>>>,
    sandbox: Mutex<Option<Arc<dyn Sandbox>>>,
    cancel: Arc<Cancel>,
    buf: Mutex<EventBuf>,
    tx: broadcast::Sender<Value>,
    ring_cap: usize,
}

fn valid_name(n: &str) -> bool { !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') }

impl Instance {
    fn push_event(&self, mut v: Value) {
        v["instance"] = json!(self.name);
        if v["agent"] == "lead" { v["agent"] = json!(self.name); }
        let mut b = self.buf.lock().unwrap();
        b.seq += 1;
        v["seq"] = json!(b.seq);
        if b.ring.len() >= self.ring_cap { b.ring.pop_front(); }
        b.ring.push_back(v.clone());
        drop(b);
        let _ = self.tx.send(v);
    }

    fn sink(self: &Arc<Self>) -> events::Sink {
        let w = Arc::downgrade(self);
        Arc::new(move |v| { if let Some(i) = w.upgrade() { i.push_event(v); } })
    }
}

impl Daemon {
    fn client_for(&self, c: &config::Config) -> Arc<Client> {
        let key = format!("{}|{}|{}", c.base_url, c.api_key, c.model);
        self.clients.lock().unwrap().entry(key).or_insert_with(|| Arc::new(Client::new(c.clone()))).clone()
    }

    fn ns(app: &str, name: &str) -> String { format!("{app}/{name}") }

    fn find(&self, app: &str, name: &str) -> Result<Arc<Instance>, String> {
        self.instances.lock().unwrap().get(&Self::ns(app, name)).cloned().ok_or_else(|| format!("no such instance `{name}`"))
    }

    /// Allocation is cheap and happens in `create`; the model clients, sandbox, session file and agent are built here, lazily.
    async fn ensure_built(self: &Arc<Self>, inst: &Arc<Instance>) -> Result<(), String> {
        let mut built = inst.build.lock().await;
        if *built { return Ok(()); }
        let p = &inst.p;
        let settings = config::Settings::from_client(p.config.clone(), p.model.as_deref()).map_err(|e| e.to_string())?;
        let (client, small) = (self.client_for(&settings.main), settings.small.as_ref().map(|c| self.client_for(c)));
        let mut policy = Policy::new(p.cwd.clone());
        let pol = &p.policy;
        let paths = |k: &str| pol.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str()).map(|s| if s.starts_with('/') { PathBuf::from(s) } else { p.cwd.join(s) }).collect::<Vec<_>>());
        if let Some(r) = paths("read") { policy.read = r; }
        if let Some(w) = paths("write") { policy.write.extend(w); }
        policy.net = pol.get("net").and_then(|v| v.as_bool()).unwrap_or(false);
        policy.outside = pol.get("outside").and_then(|v| v.as_bool()).unwrap_or(false);
        policy.scrub_env = true; // tools never inherit the daemon's environment
        let copts = tools::ContainerOpts { agent: inst.name.clone(), net: policy.net, project_rw: true, ..Default::default() };
        let (sandbox, warning) = tools::open_with(p.mode, policy.clone(), true, p.require_sandbox, copts).map_err(|e| e.to_string())?;
        let net = policy.net || p.mode == Mode::None;
        let ctx = Arc::new(tools::Ctx::with_sandbox(sandbox.clone(), net));
        let mut agent = Agent::new(client.clone(), ctx, AgentOpts {
            yes: true, allow_outside: policy.outside, guard: GuardMode::Deny, readonly: false, quiet: false, format: events::Format::Json,
            limits: p.limits.clone(), small: small.clone(), thinking: false,
        });
        agent.name = inst.name.clone();
        agent.daemon = true;
        agent.stream = p.stream;
        agent.gate = Some(self.gate.clone());
        agent.global = Some(self.global.clone());
        agent.cancel = inst.cancel.clone();
        agent.attach_session(Session::open_or_create(Some(&p.session), &p.cwd, &settings.main.model).map_err(|e| e.to_string())?, false);
        let sc = || SpawnCtx { client: client.clone(), small: small.clone(), raw: p.config.clone(), policy: policy.clone(), mode: p.mode, auto: true, require: p.require_sandbox,
            session_base: p.session.clone(), model: settings.main.model.clone(), lead_tier: sandbox.tier(), idmap_full: false, limits: p.limits.clone(), format: events::Format::Json };
        let g = |k: &str, d: u64| p.swarm.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
        let mut cfg = SwarmCfg { max_workers: p.max_workers, max_depth: g("max_depth", 1) as u32, pair_cap: g("pair_cap", 40) as u32, total_cap: g("total_cap", 400) as u32,
            chain_cap: g("chain_cap", 16) as u32, concurrency: g("concurrency", 4) as usize, show_workers: false, json: true, unbounded: p.group.is_some() };
        let swarm = if let Some(gname) = &p.group {
            let key = Self::ns(&inst.app, gname);
            let existing = self.groups.lock().unwrap().get(&key).cloned();
            match existing {
                Some(sw) => { let rx = sw.add_root(&inst.name, sc(), sandbox.clone())?; agent.join_group(sw.clone(), rx, &inst.name, gname); sw }
                None => {
                    cfg.max_workers = p.max_workers;
                    let (sw, rx) = Swarm::new(cfg, sc(), &inst.name, sandbox.clone());
                    agent.join_group(sw.clone(), rx, &inst.name, gname);
                    self.groups.lock().unwrap().insert(key, sw.clone());
                    sw
                }
            }
        } else if p.max_workers > 0 {
            let (sw, rx) = Swarm::new(cfg, sc(), &inst.name, sandbox.clone());
            agent.join_swarm_as_lead(sw.clone(), rx);
            sw
        } else {
            *inst.agent.lock().unwrap() = Some(agent);
            *inst.sandbox.lock().unwrap() = Some(sandbox);
            if let Some(w) = warning { inst.push_event(json!({"type": "warning", "message": w})); }
            *built = true;
            return Ok(());
        };
        *inst.swarm.lock().unwrap() = Some(swarm.clone());
        *inst.agent.lock().unwrap() = Some(agent);
        *inst.sandbox.lock().unwrap() = Some(sandbox);
        if let Some(w) = warning { inst.push_event(json!({"type": "warning", "message": w})); }
        *built = true;
        // a message that arrives while the lead is idle wakes it
        let (d, i) = (self.clone(), inst.clone());
        let sink = i.sink();
        tokio::spawn(events::SINK.scope(sink, async move {
            loop {
                if i.p.group.is_none() && i.p.max_workers == 0 { break; }
                let sw = i.swarm.lock().unwrap().clone();
                let Some(sw) = sw else { break };
                if !sw.has_agent(&i.name) { break; }
                sw.wait_queued(&i.name).await;
                if i.running.load(Ordering::SeqCst) { tokio::time::sleep(Duration::from_millis(50)).await; continue; }
                if d.start_turn(&i, "[system] Messages arrived; read them above, act on them, and reply.".into()).is_err() { tokio::time::sleep(Duration::from_millis(50)).await; }
            }
        }));
        Ok(())
    }

    fn start_turn(self: &Arc<Self>, inst: &Arc<Instance>, input: String) -> Result<(), String> {
        if inst.crashed.load(Ordering::SeqCst) { return Err("instance crashed earlier; close it and create a new one".into()); }
        if inst.running.swap(true, Ordering::SeqCst) { return Err("instance is busy".into()); }
        let Some(mut agent) = inst.agent.lock().unwrap().take() else { inst.running.store(false, Ordering::SeqCst); return Err("instance is not ready".into()) };
        let cap = self.cfg.max_history_bytes;
        let sink = inst.sink();
        let sid = agent.sid().to_string();
        let swarm = inst.swarm.lock().unwrap().clone();
        let name = inst.name.clone();
        if let Some(sw) = &swarm { sw.set_status(&name, crate::swarm::Status::Running); }
        let work = events::SINK.scope(sink, async move {
            #[cfg(debug_assertions)]
            if input == "__zcode_test_panic__" { panic!("test panic"); }
            if context::estimate(&agent.msgs) * 4 > cap { let _ = agent.shrink(true).await; }
            let first = agent.turn(&input).await;
            let res = crate::settle(&mut agent, first).await;
            if let Some(sw) = &swarm { sw.set_status(&name, crate::swarm::Status::Idle); }
            let (stop, code, msg, text) = match &res {
                Ok(o) => match &o.stop { Stop::Done => ("done", 0, String::new(), o.text.clone()), Stop::Limit(w) => ("limit", 3, w.clone(), o.text.clone()), Stop::Interrupted => ("interrupted", 130, String::new(), o.text.clone()) },
                Err(e) => ("error", 1, e.to_string(), String::new()),
            };
            if stop == "error" { events::error(&sid, "api", &msg); }
            events::custom(&sid, "turn_end", json!({"stop": stop, "code": code, "message": msg, "text": text}));
            agent
        });
        let (h, inst2) = (tokio::spawn(work), inst.clone());
        tokio::spawn(async move {
            match h.await {
                Ok(agent) => { *inst2.agent.lock().unwrap() = Some(agent); }
                Err(e) => {
                    // a panic stays inside this instance
                    inst2.crashed.store(true, Ordering::SeqCst);
                    inst2.push_event(json!({"type": "error", "agent": inst2.name, "error": {"type": "panic", "message": format!("instance panicked: {e}")}}));
                    inst2.push_event(json!({"type": "turn_end", "agent": inst2.name, "stop": "error", "code": 1, "message": "instance panicked", "text": ""}));
                }
            }
            inst2.running.store(false, Ordering::SeqCst);
        });
        Ok(())
    }

    async fn close_instance(&self, app: &str, name: &str) -> Result<(), String> {
        let inst = self.instances.lock().unwrap().remove(&Self::ns(app, name)).ok_or_else(|| format!("no such instance `{name}`"))?;
        inst.cancel.cancel();
        let sw = inst.swarm.lock().unwrap().take();
        if let Some(sw) = sw {
            sw.remove_root(&inst.name).await;
            if let Some(g) = &inst.p.group {
                let key = Self::ns(app, g);
                if !sw.has_roots() { self.groups.lock().unwrap().remove(&key); }
            }
        }
        let sb = inst.sandbox.lock().unwrap().take();
        if let Some(sb) = sb { sb.close().await; }
        inst.push_event(json!({"type": "closed"}));
        Ok(())
    }
}

fn reply(tx: &mpsc::UnboundedSender<Value>, req: &Value, ok: Result<Value, String>) {
    let id = req["req"].clone();
    let v = match ok {
        Ok(mut x) => { if !x.is_object() { x = json!({}); } x["req"] = id; x["ok"] = json!(true); x }
        Err(e) => json!({"req": id, "ok": false, "err": e}),
    };
    let _ = tx.send(v);
}

async fn handle_conn(d: Arc<Daemon>, stream: UnixStream) {
    match stream.peer_cred() {
        Ok(c) if c.uid() == unsafe { libc::getuid() } => {}
        _ => return,
    }
    let (mut rd, mut wr) = stream.into_split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
    tokio::spawn(async move { while let Some(v) = rx.recv().await { if write_frame(&mut wr, &v).await.is_err() { break; } } });
    let mut app = "default".to_string();
    let mut authed = d.cfg.apps.is_empty();
    let mut attached: HashMap<String, tokio::task::JoinHandle<()>> = HashMap::new();
    // instances created with `close_on_disconnect` (an interactive terminal's agent) end with their connection
    let mut owned: Vec<String> = vec![];
    while let Ok(req) = read_frame(&mut rd).await {
        let op = req["op"].as_str().unwrap_or("").to_string();
        if op == "hello" {
            let a = req["app"].as_str().unwrap_or("default").to_string();
            let ok = d.cfg.apps.is_empty() || d.cfg.apps.get(&a).is_some_and(|t| Some(t.as_str()) == req["token"].as_str());
            if !valid_name(&a) { reply(&tx, &req, Err("invalid app name".into())); continue; }
            if ok { app = a; authed = true; reply(&tx, &req, Ok(json!({}))); } else { reply(&tx, &req, Err("bad app or token".into())); }
            continue;
        }
        if !authed { reply(&tx, &req, Err("hello with app and token required".into())); continue; }
        let r = dispatch(&d, &app, &op, &req, &tx, &mut attached).await;
        if op == "create" && r.is_ok() && req["close_on_disconnect"].as_bool() == Some(true) && let Some(n) = req["instance"].as_str() { owned.push(n.to_string()); }
        reply(&tx, &req, r);
    }
    for n in owned { let _ = d.close_instance(&app, &n).await; }
    // a dead client never takes its instances down; only its event forwarders stop
    for (_, h) in attached { h.abort(); }
}

fn str_of<'a>(v: &'a Value, k: &str) -> Result<&'a str, String> { v[k].as_str().ok_or_else(|| format!("missing `{k}`")) }

async fn dispatch(d: &Arc<Daemon>, app: &str, op: &str, req: &Value, tx: &mpsc::UnboundedSender<Value>, attached: &mut HashMap<String, tokio::task::JoinHandle<()>>) -> Result<Value, String> {
    match op {
        "create" => {
            let name = str_of(req, "instance")?;
            if !valid_name(name) { return Err("instance must be [A-Za-z0-9_-]{1,32}".into()); }
            let cwd = PathBuf::from(str_of(req, "cwd")?);
            if !cwd.is_absolute() || !cwd.is_dir() { return Err("cwd must be an existing absolute directory".into()); }
            let cwd = cwd.canonicalize().map_err(|e| e.to_string())?;
            if !req["config"].is_object() { return Err("create needs a `config` object with provider and model (the daemon never uses its own credentials)".into()); }
            let mode = match req["sandbox"].as_str().unwrap_or("landlock") {
                "none" => Mode::None, "landlock" => Mode::Landlock, "warn" => Mode::Warn, "container" => Mode::Container,
                o => return Err(format!("sandbox must be none|landlock|warn|container, got `{o}`")),
            };
            let l = &req["limits"];
            let limits = Limits { max_time: l["max_time"].as_u64().filter(|n| *n > 0).map(Duration::from_secs), max_steps: l["max_steps"].as_u64().unwrap_or(60) as usize, max_tokens: l["max_tokens"].as_u64().unwrap_or(400_000) };
            let session = req["session"].as_str().map(String::from).unwrap_or_else(session::new_id);
            let mut map = d.instances.lock().unwrap();
            if map.len() >= d.cfg.max_instances { return Err(format!("instance limit reached ({})", d.cfg.max_instances)); }
            let key = Daemon::ns(app, name);
            if map.contains_key(&key) { return Err(format!("instance `{name}` already exists")); }
            let (btx, _) = broadcast::channel(1024);
            let group = req["group"].as_str().map(String::from);
            if group.as_ref().is_some_and(|g| !valid_name(g)) { return Err("invalid group name".into()); }
            let inst = Arc::new(Instance {
                name: name.to_string(), app: app.to_string(),
                p: Params { cwd, config: req["config"].clone(), model: req["model"].as_str().map(String::from), mode, require_sandbox: req["require_sandbox"].as_bool().unwrap_or(false),
                    policy: req["policy"].clone(), limits, group, max_workers: req["max_workers"].as_u64().unwrap_or(0) as usize, stream: req["stream"].as_bool().unwrap_or(false),
                    session: session.clone(), swarm: req["swarm"].clone() },
                agent: Mutex::new(None), build: tokio::sync::Mutex::new(false), running: AtomicBool::new(false), crashed: AtomicBool::new(false),
                swarm: Mutex::new(None), sandbox: Mutex::new(None), cancel: Arc::new(Cancel::default()), buf: Mutex::new(EventBuf::default()), tx: btx, ring_cap: d.cfg.ring,
            });
            map.insert(key, inst);
            Ok(json!({"instance": name, "session": session}))
        }
        "attach" => {
            let inst = d.find(app, str_of(req, "instance")?)?;
            let since = req["since"].as_u64().unwrap_or(0);
            if let Some(h) = attached.remove(&inst.name) { h.abort(); }
            // subscribe first, then replay the ring: nothing is lost, duplicates are filtered by seq
            let mut sub = inst.tx.subscribe();
            let backlog: Vec<Value> = inst.buf.lock().unwrap().ring.iter().filter(|e| e["seq"].as_u64().unwrap_or(0) > since).cloned().collect();
            let n = backlog.len();
            let txc = tx.clone();
            let h = tokio::spawn(async move {
                let mut last = since;
                for e in backlog { last = e["seq"].as_u64().unwrap_or(last); if txc.send(e).is_err() { return; } }
                loop {
                    match sub.recv().await {
                        Ok(e) => { let s = e["seq"].as_u64().unwrap_or(0); if s > last { last = s; if txc.send(e).is_err() { return; } } }
                        Err(broadcast::error::RecvError::Lagged(_)) => {}
                        Err(_) => return,
                    }
                }
            });
            attached.insert(inst.name.clone(), h);
            Ok(json!({"replayed": n}))
        }
        "detach" => {
            let n = str_of(req, "instance")?;
            if let Some(h) = attached.remove(n) { h.abort(); }
            Ok(json!({}))
        }
        "prompt" => {
            let inst = d.find(app, str_of(req, "instance")?)?;
            let text = str_of(req, "text")?.to_string();
            if text.trim().is_empty() { return Err("empty prompt".into()); }
            d.ensure_built(&inst).await?;
            // a person typed: re-arm the agent-to-agent message caps and the reply-chain counter
            if let Some(sw) = inst.swarm.lock().unwrap().clone() { sw.reset_caps(); }
            if let Some(a) = inst.agent.lock().unwrap().as_mut() { a.last_hops = 0; }
            d.start_turn(&inst, text)?;
            Ok(json!({}))
        }
        "build" => {
            // join the group (and start answering messages) now instead of at the first prompt
            let inst = d.find(app, str_of(req, "instance")?)?;
            d.ensure_built(&inst).await?;
            let agents = inst.swarm.lock().unwrap().clone().map(|s| s.list()).unwrap_or_default();
            Ok(json!({"agents": agents}))
        }
        "send" => {
            let inst = d.find(app, str_of(req, "instance")?)?;
            d.ensure_built(&inst).await?;
            let sw = inst.swarm.lock().unwrap().clone().ok_or("this instance has no swarm (create it with max_workers or a group)")?;
            sw.send("client", str_of(req, "to")?, str_of(req, "text")?, 0, None).map(|m| json!({"result": m}))
        }
        "spawn" => {
            let inst = d.find(app, str_of(req, "instance")?)?;
            d.ensure_built(&inst).await?;
            let sw = inst.swarm.lock().unwrap().clone().ok_or("this instance has no swarm (create it with max_workers)")?;
            let write: Vec<String> = req["write"].as_array().map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
            let sink = inst.sink();
            let r = events::SINK.scope(sink, sw.spawn(&inst.name, str_of(req, "name")?, str_of(req, "prompt")?, &write, req["profile"].as_str())).await;
            r.map(|m| json!({"result": m}))
        }
        "cancel" => { d.find(app, str_of(req, "instance")?)?.cancel.cancel(); Ok(json!({})) }
        "close" => {
            let n = str_of(req, "instance")?.to_string();
            if let Some(h) = attached.remove(&n) { h.abort(); }
            d.close_instance(app, &n).await?;
            Ok(json!({}))
        }
        "status" => {
            let list: Vec<Arc<Instance>> = match req["instance"].as_str() {
                Some(n) => vec![d.find(app, n)?],
                None => d.instances.lock().unwrap().values().filter(|i| i.app == app).cloned().collect(),
            };
            let items: Vec<Value> = list.iter().map(|i| {
                let sw = i.swarm.lock().unwrap().clone();
                json!({"instance": i.name, "cwd": i.p.cwd, "group": i.p.group, "running": i.running.load(Ordering::SeqCst), "crashed": i.crashed.load(Ordering::SeqCst),
                    "built": i.agent.lock().unwrap().is_some() || i.running.load(Ordering::SeqCst), "events": i.buf.lock().unwrap().seq,
                    "agents": sw.map(|s| s.list()).unwrap_or_default()})
            }).collect();
            Ok(json!({"instances": items}))
        }
        o => Err(format!("unknown op `{o}`")),
    }
}

pub async fn run() -> anyhow::Result<i32> {
    let raw = config::layers();
    let sc = raw.get("serve").cloned().unwrap_or(Value::Null);
    let g = |k: &str, d: u64| sc.get(k).and_then(|v| v.as_u64()).unwrap_or(d);
    let apps: HashMap<String, String> = sc.get("apps").and_then(|v| v.as_object()).map(|o| o.iter().map(|(k, v)| (k.clone(), v["token"].as_str().unwrap_or("").to_string())).collect()).unwrap_or_default();
    let global = Budget::new(&Limits { max_time: None, max_steps: g("max_steps", u64::MAX / 2) as usize, max_tokens: g("max_tokens", u64::MAX / 2) });
    let d = Arc::new(Daemon {
        cfg: ServeCfg { max_instances: g("max_instances", 64) as usize, max_history_bytes: g("max_history_bytes", 8 << 20) as usize, ring: g("event_ring", 2000) as usize, apps },
        instances: Default::default(), groups: Default::default(), clients: Default::default(),
        gate: Arc::new(Semaphore::new(g("concurrency", 8) as usize)), global: Arc::new(global),
    });
    tools::container::configure(tools::container::ContainerCfg::from_config(&raw, crate::guest_embed::GUEST, false, false).map_err(anyhow::Error::msg)?);
    let path = socket_path();
    if let Some(dir) = path.parent() {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    }
    // refuse to steal the socket of a live daemon; clean up a stale one
    if UnixStream::connect(&path).await.is_ok() { anyhow::bail!("a zcode daemon is already running at {}", path.display()); }
    let _ = std::fs::remove_file(&path);
    let listener = UnixListener::bind(&path)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    if cfg!(panic = "abort") {
        eprintln!("note: built with panic=abort: a panic inside one instance will stop the whole daemon; build with `cargo build --profile serve` for per-instance panic isolation");
    }
    eprintln!("zcode daemon listening on {}", path.display());
    loop {
        tokio::select! {
            r = listener.accept() => {
                let Ok((s, _)) = r else { continue };
                let d = d.clone();
                tokio::spawn(async move { handle_conn(d, s).await });
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = sigterm() => break,
        }
    }
    let names: Vec<(String, String)> = d.instances.lock().unwrap().values().map(|i| (i.app.clone(), i.name.clone())).collect();
    for (a, n) in names { let _ = d.close_instance(&a, &n).await; }
    let _ = std::fs::remove_file(&path);
    Ok(0)
}

async fn sigterm() {
    use tokio::signal::unix::{SignalKind, signal};
    match signal(SignalKind::terminate()) { Ok(mut s) => { s.recv().await; } Err(_) => std::future::pending::<()>().await }
}
