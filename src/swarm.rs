//! Single-process swarm: every agent is an async task with its own history, session, inbox, Policy and
//! Sandbox. One in-memory bus (`mpsc::UnboundedSender<Msg>` per agent name), push delivery (the agent
//! drains its inbox before each model call), swarm-wide budgets and one shared model-request semaphore.
use crate::agent::{Agent, AgentOpts, Limits, Stop};
use crate::events;
use crate::guard::GuardMode;
use crate::llm::Client;
use crate::session::Session;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tokio::sync::{Notify, Semaphore, mpsc};
use zcode_tools::{self as tools, Mode, Policy, Sandbox, Tier};
use tools::container::parse_size;

#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct Msg {
    pub id: u64,
    pub from: String,
    pub to: String,
    pub in_reply_to: Option<u64>,
    pub text: String,
    pub hops: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status { Running, Idle, Waiting }
impl Status {
    fn name(self) -> &'static str { match self { Status::Running => "running", Status::Idle => "idle", Status::Waiting => "waiting" } }
}

#[derive(Clone, Debug)]
pub struct SwarmCfg {
    pub max_workers: usize,
    /// 1 = only the lead may spawn
    pub max_depth: u32,
    /// messages allowed between one pair of agents
    pub pair_cap: u32,
    /// messages allowed in the whole swarm
    pub total_cap: u32,
    /// longest reply chain (a message answering a message answering ...)
    pub chain_cap: u32,
    pub concurrency: usize,
    pub show_workers: bool,
    pub json: bool,
}
impl Default for SwarmCfg {
    fn default() -> Self {
        SwarmCfg { max_workers: 4, max_depth: 1, pair_cap: 40, total_cap: 400, chain_cap: 16, concurrency: 4, show_workers: false, json: false }
    }
}

/// Budgets shared by the whole swarm (`--max-steps/--max-tokens/--max-time`).
pub struct Budget {
    steps: AtomicU64,
    tokens: AtomicU64,
    max_steps: u64,
    max_tokens: u64,
    pub deadline: Option<tokio::time::Instant>,
}
impl Budget {
    pub fn new(l: &Limits) -> Self {
        Budget { steps: AtomicU64::new(0), tokens: AtomicU64::new(0), max_steps: l.max_steps as u64, max_tokens: l.max_tokens,
            deadline: l.max_time.map(|d| tokio::time::Instant::now() + d) }
    }
    pub fn step(&self) -> Result<(), String> {
        if self.steps.fetch_add(1, Ordering::Relaxed) >= self.max_steps { return Err("swarm step budget exhausted".into()); }
        if self.tokens.load(Ordering::Relaxed) > self.max_tokens { return Err("swarm token budget exhausted".into()); }
        if self.deadline.is_some_and(|d| tokio::time::Instant::now() >= d) { return Err("swarm time limit reached".into()); }
        Ok(())
    }
    pub fn tokens(&self, n: u64) { self.tokens.fetch_add(n, Ordering::Relaxed); }
    pub fn used(&self) -> (u64, u64) { (self.steps.load(Ordering::Relaxed), self.tokens.load(Ordering::Relaxed)) }
}

struct Entry {
    /// how this agent's root was set up (workers inherit their parent's)
    ctx: Arc<SpawnCtx>,
    tx: mpsc::UnboundedSender<Msg>,
    status: Status,
    queued: usize,
    parent: Option<String>,
    tier: Tier,
    sandbox: Arc<dyn Sandbox>,
    handle: Option<tokio::task::JoinHandle<()>>,
    steps: u64,
    tin: u64,
    tout: u64,
    write: Vec<PathBuf>,
}

#[derive(Default)]
struct State {
    agents: BTreeMap<String, Entry>,
    in_flight: usize,
    pair: HashMap<(String, String), u32>,
    total: u32,
    /// bumped each time some waiter observes the swarm idle, so every parked waiter sees it (no starvation)
    idle_epoch: HashMap<String, u64>,
}
impl State {
    /// root of `me`'s tree: the agent itself, or its parent for a worker
    fn root_of(&self, me: &str) -> String {
        self.agents.get(me).and_then(|a| a.parent.clone()).unwrap_or_else(|| me.to_string())
    }
    /// every agent of one instance (the root and its workers); with a single root this is the whole swarm
    fn tree(&self, root: &str) -> impl Iterator<Item = (&String, &Entry)> {
        let r = root.to_string();
        self.agents.iter().filter(move |(k, a)| **k == r || a.parent.as_deref() == Some(r.as_str()))
    }
    fn idle_tree(&self, root: &str) -> bool { self.tree(root).all(|(_, a)| a.status != Status::Running && a.queued == 0) }
}

/// What a worker needs to be built (captured from the lead's setup).
pub struct SpawnCtx {
    pub client: Arc<Client>,
    pub small: Option<Arc<Client>>,
    pub raw: Value,
    pub policy: Policy,
    pub mode: Mode,
    pub auto: bool,
    pub require: bool,
    pub session_base: String,
    pub model: String,
    pub lead_tier: Tier,
    pub idmap_full: bool,
    pub limits: Limits,
    pub format: events::Format,
}

pub struct Swarm {
    st: Mutex<State>,
    pub cfg: SwarmCfg,
    notify: Notify,
    next_id: AtomicU64,
    pub budget: Budget,
    pub sem: Arc<Semaphore>,
    pub sc: SpawnCtx,
    print: Mutex<()>,
}

fn tier_of(m: Mode) -> Tier { match m { Mode::None => Tier::None, Mode::Landlock | Mode::Warn => Tier::Landlock, Mode::Container => Tier::Container } }

fn valid_name_any(n: &str) -> bool { !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') }

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 32 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && n != "lead"
}

pub fn tool_schemas(can_spawn: bool) -> Vec<Value> {
    let f = |name: &str, desc: &str, params: Value| json!({"type":"function","function":{"name":name,"description":desc,"parameters":params}});
    let mut v = vec![];
    if can_spawn {
        v.push(f("spawn", "Start a worker agent; returns at once. write: project paths it may modify. profile: named config profile.",
            json!({"type":"object","properties":{"name":{"type":"string"},"prompt":{"type":"string"},"write":{"type":"array","items":{"type":"string"}},"profile":{"type":"string"}},"required":["name","prompt"]})));
    }
    v.push(f("send", "Message an agent by name, or * for all. Send summaries and file paths under the project dir, not file contents. Never returns the reply; it arrives later.",
        json!({"type":"object","properties":{"to":{"type":"string"},"text":{"type":"string"}},"required":["to","text"]})));
    v.push(f("wait", "Wait until a message arrives or every agent is idle.", json!({"type":"object","properties":{}})));
    v
}

impl Swarm {
    pub fn new(cfg: SwarmCfg, sc: SpawnCtx, root: &str, lead_sandbox: Arc<dyn Sandbox>) -> (Arc<Swarm>, mpsc::UnboundedReceiver<Msg>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let mut st = State::default();
        let sc_arc = Arc::new(SpawnCtx { client: sc.client.clone(), small: sc.small.clone(), raw: sc.raw.clone(), policy: sc.policy.clone(), mode: sc.mode,
            auto: sc.auto, require: sc.require, session_base: sc.session_base.clone(), model: sc.model.clone(), lead_tier: sc.lead_tier, idmap_full: sc.idmap_full,
            limits: sc.limits.clone(), format: sc.format });
        st.agents.insert(root.into(), Entry { ctx: sc_arc, tx, status: Status::Running, queued: 0, parent: None, tier: lead_sandbox.tier(), sandbox: lead_sandbox,
            handle: None, steps: 0, tin: 0, tout: 0, write: vec![] });
        let sw = Swarm { st: Mutex::new(st), sem: Arc::new(Semaphore::new(cfg.concurrency.max(1))), budget: Budget::new(&sc.limits), cfg, notify: Notify::new(),
            next_id: AtomicU64::new(1), sc, print: Mutex::new(()) };
        (Arc::new(sw), rx)
    }

    /// Join another root agent (a daemon instance in the same group) to this bus.
    pub fn add_root(&self, name: &str, sc: SpawnCtx, sandbox: Arc<dyn Sandbox>) -> Result<mpsc::UnboundedReceiver<Msg>, String> {
        if !valid_name_any(name) { return Err(format!("invalid agent name `{name}`")); }
        let mut st = self.st.lock().unwrap();
        if st.agents.contains_key(name) { return Err(format!("agent `{name}` already exists in this group")); }
        let (tx, rx) = mpsc::unbounded_channel();
        st.agents.insert(name.into(), Entry { ctx: Arc::new(sc), tx, status: Status::Running, queued: 0, parent: None, tier: sandbox.tier(), sandbox,
            handle: None, steps: 0, tin: 0, tout: 0, write: vec![] });
        Ok(rx)
    }

    /// Drop a root (its instance was closed) and everything it spawned.
    pub async fn remove_root(&self, name: &str) {
        type Item = (Option<tokio::task::JoinHandle<()>>, Arc<dyn Sandbox>);
        let items: Vec<Item> = {
            let mut st = self.st.lock().unwrap();
            let names: Vec<String> = st.agents.iter().filter(|(k, a)| *k == name || a.parent.as_deref() == Some(name)).map(|(k, _)| k.clone()).collect();
            names.iter().filter_map(|n| st.agents.remove(n)).map(|mut a| (a.handle.take(), a.sandbox)).filter(|_| true).collect()
        };
        for (h, sb) in items { if let Some(h) = h { h.abort(); } sb.close().await; }
        self.notify.notify_waiters();
    }

    fn ev(&self, kind: &str, extra: Value) {
        if self.cfg.json { events::custom(&self.sc.session_base, kind, extra); }
    }

    pub fn show(&self, name: &str, line: &str) {
        if !self.cfg.show_workers || self.cfg.json { return; }
        let _g = self.print.lock().unwrap();
        for l in line.lines().take(20) { eprintln!("\x1b[2m[{name}] {l}\x1b[0m"); }
    }

    pub fn set_status(&self, name: &str, s: Status) {
        let mut st = self.st.lock().unwrap();
        if let Some(a) = st.agents.get_mut(name) { a.status = s; }
        drop(st);
        self.notify.notify_waiters();
    }

    /// Called when an agent moved `n` queued messages into its history.
    pub fn consumed(&self, name: &str, n: usize) {
        let mut st = self.st.lock().unwrap();
        if let Some(a) = st.agents.get_mut(name) { a.queued = a.queued.saturating_sub(n); }
        st.in_flight = st.in_flight.saturating_sub(n);
        drop(st);
        self.notify.notify_waiters();
    }

    pub fn report(&self, name: &str, tin: u64, tout: u64) {
        if let Some(a) = self.st.lock().unwrap().agents.get_mut(name) { a.steps += 1; a.tin += tin; a.tout += tout; }
    }

    /// Non-blocking send. `hops` is the chain depth of the message being answered (0 for fresh).
    pub fn send(&self, from: &str, to: &str, text: &str, last_hops: u32, in_reply_to: Option<u64>) -> Result<String, String> {
        if text.trim().is_empty() { return Err("empty message".into()); }
        let mut st = self.st.lock().unwrap();
        let targets: Vec<String> = if to == "*" { st.agents.keys().filter(|k| *k != from).cloned().collect() }
            else if to == from { return Err("cannot send to yourself".into()) }
            else if st.agents.contains_key(to) { vec![to.to_string()] }
            else { return Err(format!("no such agent `{to}` (agents: {})", st.agents.keys().cloned().collect::<Vec<_>>().join(", "))) };
        if targets.is_empty() { return Err("no other agents".into()); }
        let hops = last_hops + 1;
        if hops > self.cfg.chain_cap { return Err(format!("hop limit reached ({} chained messages); stop replying and finish your task", self.cfg.chain_cap)); }
        // check every cap before delivering to anyone
        for t in &targets {
            let key = if from < t.as_str() { (from.to_string(), t.clone()) } else { (t.clone(), from.to_string()) };
            if st.pair.get(&key).copied().unwrap_or(0) >= self.cfg.pair_cap { return Err(format!("hop limit reached between {from} and {t} ({} messages)", self.cfg.pair_cap)); }
        }
        if st.total as usize + targets.len() > self.cfg.total_cap as usize { return Err(format!("global message limit reached ({})", self.cfg.total_cap)); }
        for t in &targets { self.deliver(&mut st, from, t, text, hops, in_reply_to); }
        drop(st);
        self.notify.notify_waiters();
        Ok(format!("sent to {}", targets.join(", ")))
    }

    fn deliver(&self, st: &mut State, from: &str, to: &str, text: &str, hops: u32, in_reply_to: Option<u64>) {
        let key = if from < to { (from.to_string(), to.to_string()) } else { (to.to_string(), from.to_string()) };
        *st.pair.entry(key).or_insert(0) += 1;
        st.total += 1;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = Msg { id, from: from.into(), to: to.into(), in_reply_to, text: text.into(), hops };
        let Some(a) = st.agents.get_mut(to) else { return };
        if a.tx.send(msg).is_ok() {
            a.queued += 1;
            if a.status == Status::Idle { a.status = Status::Running; }
            st.in_flight += 1;
            self.ev("message", json!({"agent": from, "from": from, "to": to, "id": id, "hops": hops, "text": snippet(text)}));
        }
    }

    /// Park until a message is queued for `me` or the whole swarm is idle.
    pub async fn wait(&self, me: &str) -> String {
        self.set_status(me, Status::Waiting);
        let (root, start_epoch) = { let st = self.st.lock().unwrap(); let r = st.root_of(me); let e = st.idle_epoch.get(&r).copied().unwrap_or(0); (r, e) };
        loop {
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            {
                let mut st = self.st.lock().unwrap();
                let q = st.agents.get(me).map(|a| a.queued).unwrap_or(0);
                let idle = st.idle_tree(&root);
                if idle { *st.idle_epoch.entry(root.clone()).or_insert(0) += 1; self.notify.notify_waiters(); }
                let idle = idle || st.idle_epoch.get(&root).copied().unwrap_or(0) != start_epoch;
                if q > 0 || idle {
                    if let Some(a) = st.agents.get_mut(me) { a.status = Status::Running; }
                    return if q > 0 { format!("{q} message(s) arrived") } else { "swarm idle: all agents are idle, no messages pending".into() };
                }
            }
            n.await;
        }
    }

    pub fn has_agent(&self, name: &str) -> bool { self.st.lock().unwrap().agents.contains_key(name) }
    pub fn has_roots(&self) -> bool { self.st.lock().unwrap().agents.values().any(|a| a.parent.is_none()) }

    /// Park until `me` has a queued message (does not return on mere idleness).
    pub async fn wait_queued(&self, me: &str) {
        loop {
            let n = self.notify.notified();
            tokio::pin!(n);
            n.as_mut().enable();
            if self.st.lock().unwrap().agents.get(me).map(|a| a.queued > 0).unwrap_or(true) { return; }
            n.await;
        }
    }

    /// True while some worker is running or `me` still has unread messages.
    pub fn active(&self, me: &str) -> bool {
        let st = self.st.lock().unwrap();
        let root = st.root_of(me);
        st.tree(&root).any(|(k, a)| (k != me && a.status == Status::Running) || a.queued > 0)
    }

    /// A worker's turn ended: deliver its final answer to the parent and go idle (atomically).
    fn finish_turn(&self, name: &str, text: &str, hops: u32) {
        let mut st = self.st.lock().unwrap();
        let parent = st.agents.get(name).and_then(|a| a.parent.clone());
        if let Some(p) = parent && st.agents.contains_key(&p) { self.deliver(&mut st, name, &p, text, hops + 1, None); }
        if let Some(a) = st.agents.get_mut(name) { a.status = Status::Idle; }
        drop(st);
        self.ev("idle", json!({"agent": name}));
        self.notify.notify_waiters();
    }

    pub fn list(&self) -> Vec<String> {
        let st = self.st.lock().unwrap();
        let (bs, bt) = self.budget.used();
        let mut v: Vec<String> = st.agents.iter().map(|(k, a)| format!("{k:<10} tier {} {:<8} queued {} steps {} tokens {}/{}{}{}", a.tier.n(), a.status.name(), a.queued, a.steps, a.tin, a.tout,
            if a.write.is_empty() { String::new() } else { format!(" write {}", a.write.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(",")) },
            { let p = a.sandbox.ports(); if p.is_empty() { String::new() } else { format!(" ports {}", p.iter().map(|(g, h)| format!("{h}->{g}")).collect::<Vec<_>>().join(",")) } })).collect();
        v.push(format!("swarm budget: {bs} steps, {bt} prompt tokens"));
        v
    }

    pub fn totals(&self) -> Vec<(String, u64, u64)> {
        self.st.lock().unwrap().agents.iter().map(|(k, a)| (k.clone(), a.tin, a.tout)).collect()
    }

    /// Stop all workers and release their sandboxes.
    pub async fn shutdown(&self) {
        type Item = (Option<tokio::task::JoinHandle<()>>, Arc<dyn Sandbox>);
        let items: Vec<Item> = {
            let mut st = self.st.lock().unwrap();
            st.agents.iter_mut().filter(|(k, _)| *k != "lead").map(|(_, a)| (a.handle.take(), a.sandbox.clone())).collect()
        };
        for (h, sb) in items {
            if let Some(h) = h { h.abort(); }
            sb.close().await;
        }
    }

    pub async fn spawn(self: &Arc<Self>, parent: &str, name: &str, prompt: &str, write: &[String], profile: Option<&str>) -> Result<String, String> {
        if !valid_name(name) { return Err(format!("invalid agent name `{name}` (letters, digits, _ and -; not `lead`)")); }
        {
            let st = self.st.lock().unwrap();
            if st.agents.contains_key(name) { return Err(format!("agent `{name}` already exists")); }
            if st.agents.values().filter(|a| a.parent.is_some()).count() >= self.cfg.max_workers { return Err(format!("worker limit reached ({})", self.cfg.max_workers)); }
        }
        let sc = self.st.lock().unwrap().agents.get(parent).map(|a| a.ctx.clone()).ok_or_else(|| format!("unknown parent `{parent}`"))?;
        let prof = match profile {
            Some(p) => sc.raw.get("profiles").and_then(|x| x.get(p)).cloned().ok_or_else(|| format!("unknown profile `{p}`"))?,
            None => Value::Null,
        };
        // tier: the session tier is the minimum; a profile may raise it, never lower it
        let session_tier = tier_of(sc.mode);
        let mode = match prof.get("sandbox").and_then(|v| v.as_str()) {
            None => sc.mode,
            Some(s) => {
                let m = match s { "none" => Mode::None, "landlock" => Mode::Landlock, "warn" => Mode::Warn, "container" => Mode::Container, o => return Err(format!("profile sandbox `{o}` is not none|landlock|warn|container")) };
                if tier_of(m) < session_tier { return Err(format!("profile `{}` asks for tier {} below the session tier {}; a profile may only raise the tier", profile.unwrap_or(""), tier_of(m).n(), session_tier.n())); }
                m
            }
        };
        // write paths: must be inside the project root; created on the host when missing
        let root = sc.policy.cwd.clone();
        let mut wpaths = vec![];
        for w in write {
            let abs = if Path::new(w).is_absolute() { PathBuf::from(w) } else { root.join(w) };
            if !crate::guard::inside(&root, &abs) { return Err(format!("write path `{w}` is outside the project directory")); }
            if !abs.exists() {
                let is_file = abs.file_name().is_some_and(|n| n.to_string_lossy().contains('.')) && !w.ends_with('/');
                let r = if is_file { abs.parent().map(std::fs::create_dir_all).transpose().and_then(|_| std::fs::File::create(&abs).map(|_| ())) } else { std::fs::create_dir_all(&abs) };
                r.map_err(|e| format!("cannot create write path `{w}`: {e}"))?;
            }
            wpaths.push(abs.canonicalize().unwrap_or(abs));
        }
        let mut policy = sc.policy.clone();
        policy.write = wpaths.clone();
        policy.outside = false;
        policy.net = prof.get("net").and_then(|v| v.as_bool()).unwrap_or(false);
        let copts = tools::ContainerOpts {
            agent: name.to_string(),
            image: prof.get("image").and_then(|v| v.as_str()).map(String::from),
            net: policy.net,
            mem: prof.get("mem").and_then(|v| v.as_str().map(String::from).or_else(|| v.as_u64().map(|n| n.to_string()))).and_then(|s| parse_size(&s)),
            cpus: prof.get("cpus").and_then(|v| v.as_f64()),
            pids: prof.get("pids").and_then(|v| v.as_u64()),
            ports: prof.get("ports").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_u64().map(|p| p as u16)).collect()).unwrap_or_default(),
            project_rw: false,
        };
        if mode == Mode::Container && sc.lead_tier < Tier::Container && sc.idmap_full {
            eprintln!("warning: container.idmap is \"full\" while the lead runs on the host: files written by container workers will not be owned by the host user; use idmap \"single\" for mixed tiers");
        }
        let (sandbox, _warn) = tools::open_with(mode, policy.clone(), sc.auto, sc.require, copts).map_err(|e| e.to_string())?;
        let ctx = Arc::new(tools::Ctx::with_sandbox(sandbox.clone(), policy.net || mode == Mode::None));
        let mut limits = sc.limits.clone();
        limits.max_time = None; // the swarm deadline applies
        let cap = (prof.get("max_steps").and_then(|v| v.as_u64()), prof.get("max_tokens").and_then(|v| v.as_u64()));
        let mut agent = Agent::new(sc.client.clone(), ctx, AgentOpts {
            yes: true, allow_outside: false, guard: GuardMode::Deny, readonly: false, quiet: true, format: sc.format, limits,
            small: sc.small.clone(), thinking: false,
        });
        let sess = Session::open_worker(&sc.session_base, name, &root, &sc.model).map_err(|e| e.to_string())?;
        agent.attach_session(sess, false);
        if cap.0.is_some() || cap.1.is_some() { agent.cap = Some(cap); }
        let (tx, rx) = mpsc::unbounded_channel();
        agent.become_worker(name, 1, self.clone(), rx, parent);
        {
            let mut st = self.st.lock().unwrap();
            if st.agents.contains_key(name) { return Err(format!("agent `{name}` already exists")); }
            st.agents.insert(name.to_string(), Entry { ctx: sc.clone(), tx, status: Status::Running, queued: 0, parent: Some(parent.into()), tier: sandbox.tier(), sandbox: sandbox.clone(),
                handle: None, steps: 0, tin: 0, tout: 0, write: wpaths });
        }
        self.ev("spawn", json!({"agent": name, "parent": parent, "tier": sandbox.tier().n(), "write": write, "profile": profile}));
        if sandbox.tier() == Tier::Container {
            // report forwarded ports once the container is up; never blocks the agent loop
            let (sb, sw, nm) = (sandbox.clone(), self.clone(), name.to_string());
            let sink = events::current_sink();
            let job = async move {
                if sb.ready().await.is_ok() {
                    for (g, h) in sb.ports() { sw.ev("port", json!({"agent": nm, "guest": g, "host": h})); }
                }
            };
            tokio::spawn(async move { match sink { Some(s) => events::SINK.scope(s, job).await, None => job.await } });
        }
        let first = format!("[from {parent}] {prompt}");
        let sw = self.clone();
        let nm = name.to_string();
        let sink = events::current_sink();
        let h = tokio::spawn(async move {
            match sink { Some(s) => events::SINK.scope(s, worker_loop(agent, sw, nm, first)).await, None => worker_loop(agent, sw, nm, first).await }
        });
        if let Some(a) = self.st.lock().unwrap().agents.get_mut(name) { a.handle = Some(h); }
        self.show(name, &format!("spawned (tier {})", sandbox.tier().n()));
        Ok(format!("spawned `{name}` (tier {}); it runs in the background. Use wait() for its report.", sandbox.tier().n()))
    }
}

fn snippet(t: &str) -> String { t.chars().take(200).collect() }

async fn worker_loop(mut agent: Agent, sw: Arc<Swarm>, name: String, first: String) {
    let mut input = first;
    loop {
        let res = agent.turn(&input).await;
        let text = match res {
            Ok(o) => match &o.stop {
                Stop::Done => o.text,
                // a worker that stopped before saying anything reports just the reason
                Stop::Limit(w) => format!("{}\n[stopped: {w}]", o.text).trim_start().to_string(),
                Stop::Interrupted => format!("{}\n[interrupted]", o.text).trim_start().to_string(),
            },
            Err(e) => format!("Error: {e}"),
        };
        sw.show(&name, &format!("done: {}", text.lines().next().unwrap_or("")));
        sw.finish_turn(&name, if text.trim().is_empty() { "(no output)" } else { &text }, agent.last_hops);
        match agent.recv_blocking().await {
            Some(m) => input = format!("[from {}] {}", m.from, m.text),
            None => break,
        }
    }
}

