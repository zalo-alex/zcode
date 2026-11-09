//! Tier 2: one rootless OCI container per agent. All tools run inside it through `zcode-guest`.
//!
//! Flow per agent (in the background, overlapping the agent's first model request):
//! per-agent dirs -> `RootfsProvider` plan -> bundle `config.json` -> unix socket -> `zcode __ns`
//! supervisor (userns + mount namespace + overlay, then OCI `create`/`start`) -> optional `pasta` ->
//! guest connects out -> ready. The first tool call awaits the ready future.
mod image;
mod oci;
mod probe;
#[cfg(feature = "oci")]
pub mod registry;
mod rootfs;
mod supervisor;

#[cfg(feature = "oci")]
pub use registry::{gc as image_gc, pull as image_pull};
pub use image::{images_dir, import as image_import, list as image_list, remove as image_remove, rootfs_of};
pub use supervisor::maybe_run as maybe_supervisor;

use crate::frames::{read_frame, write_frame};
use crate::sandbox::*;
use anyhow::{Result, anyhow};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::io::AsyncBufReadExt;
use tokio::sync::{mpsc, watch};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Idmap { Single, Full }

#[derive(Clone, Debug)]
pub struct ContainerCfg {
    pub runtime: String,
    /// extra global runtime arguments (default for crun: `--cgroup-manager=disabled`)
    pub runtime_args: Vec<String>,
    pub image: String,
    pub idmap: Idmap,
    /// `auto | overlay | fuse | copy`
    pub rootfs: String,
    pub seccomp: bool,
    pub readonly_rootfs: bool,
    pub port_range: (u16, u16),
    pub keep_state: bool,
    pub require_limits: bool,
    pub pasta: String,
    /// `auto` (pasta, else slirp4netns) | `pasta` | `slirp4netns`
    pub net_backend: String,
    pub slirp: String,
    pub guest: &'static [u8],
}

impl ContainerCfg {
    pub fn from_config(raw: &Value, guest: &'static [u8], keep_state: bool, require_limits: bool) -> Result<ContainerCfg, String> {
        let c = raw.get("container").cloned().unwrap_or(Value::Null);
        let s = |k: &str, d: &str| c.get(k).and_then(|v| v.as_str()).unwrap_or(d).to_string();
        let runtime = s("runtime", "crun");
        let idmap = match s("idmap", "single").as_str() { "single" => Idmap::Single, "full" => Idmap::Full, o => return Err(format!("container.idmap must be single|full, got `{o}`")) };
        let runtime_args = match c.get("runtime_args").and_then(|v| v.as_array()) {
            Some(a) => a.iter().filter_map(|x| x.as_str().map(String::from)).collect(),
            None if Path::new(&runtime).file_name().is_some_and(|n| n == "crun") => vec!["--cgroup-manager=disabled".to_string()],
            None => vec![],
        };
        if !matches!(s("net_backend", "auto").as_str(), "auto" | "pasta" | "slirp4netns") {
            return Err(format!("container.net_backend must be auto|pasta|slirp4netns, got `{}`", s("net_backend", "auto")));
        }
        let pr = c.get("port_range").and_then(|v| v.as_array()).map(|a| (a.first().and_then(|x| x.as_u64()).unwrap_or(20000) as u16, a.get(1).and_then(|x| x.as_u64()).unwrap_or(29999) as u16)).unwrap_or((20000, 29999));
        Ok(ContainerCfg {
            runtime, runtime_args, image: s("image", "alpine"), idmap, rootfs: s("rootfs", "auto"),
            seccomp: c.get("seccomp").and_then(|v| v.as_str()) != Some("none"),
            readonly_rootfs: c.get("readonly_rootfs").and_then(|v| v.as_bool()).unwrap_or(false),
            port_range: pr, keep_state: keep_state || c.get("keep_state").and_then(|v| v.as_bool()).unwrap_or(false),
            require_limits: require_limits || c.get("require_limits").and_then(|v| v.as_bool()).unwrap_or(false),
            pasta: s("pasta", "pasta"), net_backend: s("net_backend", "auto"), slirp: s("slirp4netns", "slirp4netns"), guest,
        })
    }
}

static CFG: OnceLock<ContainerCfg> = OnceLock::new();
pub fn configure(cfg: ContainerCfg) { let _ = CFG.set(cfg); }

pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_lowercase();
    let (n, mul) = match s.chars().last()? {
        'k' => (&s[..s.len() - 1], 1u64 << 10), 'm' => (&s[..s.len() - 1], 1 << 20), 'g' => (&s[..s.len() - 1], 1 << 30), 't' => (&s[..s.len() - 1], 1 << 40),
        _ => (&s[..], 1),
    };
    n.parse::<f64>().ok().map(|x| (x * mul as f64) as u64)
}

/// Recursively remove, fixing permissions first (overlayfs work dirs are mode 000).
pub fn force_remove(p: &Path) {
    fn chmod_tree(p: &Path) {
        if let Ok(m) = std::fs::symlink_metadata(p) && m.is_dir() {
            let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o700));
            if let Ok(rd) = std::fs::read_dir(p) { for e in rd.flatten() { chmod_tree(&e.path()); } }
        }
    }
    chmod_tree(p);
    if std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false) { let _ = std::fs::remove_dir_all(p); } else { let _ = std::fs::remove_file(p); }
}

// ---------------------------------------------------------------- run directory, locking, reaping

struct RunDir {
    id: String,
    state: PathBuf,
    sock: PathBuf,
    _lock: std::fs::File,
}

fn state_base() -> PathBuf {
    std::env::var("XDG_STATE_HOME").map(PathBuf::from).unwrap_or_else(|_| crate::sandbox::home().join(".local/state")).join("zcode")
}
fn sock_base() -> PathBuf {
    match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() && Path::new(&d).is_dir() => PathBuf::from(d).join("zcode"),
        _ => PathBuf::from(format!("/tmp/zcode-{}", unsafe { libc::getuid() })),
    }
}
fn mkdir_private(p: &Path) -> std::io::Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(p)
}

fn run_dir(cfg: &ContainerCfg) -> Result<&'static RunDir, String> {
    static R: OnceLock<Result<RunDir, String>> = OnceLock::new();
    R.get_or_init(|| {
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
        let id = format!("r{}-{:x}", std::process::id(), nanos);
        let (state, sock) = (state_base().join(&id), sock_base().join(&id));
        mkdir_private(&state).map_err(|e| format!("{}: {e}", state.display()))?;
        mkdir_private(&sock).map_err(|e| format!("{}: {e}", sock.display()))?;
        let lock = std::fs::File::create(state.join("lock")).map_err(|e| e.to_string())?;
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&lock), libc::LOCK_EX | libc::LOCK_NB) } != 0 { return Err("cannot lock run dir".into()); }
        reap_stale(&id, cfg);
        Ok(RunDir { id, state, sock, _lock: lock })
    }).as_ref().map_err(|e| e.clone())
}

/// Remove containers, pasta processes and dirs left behind by runs that no longer hold their lock.
fn reap_stale(own: &str, cfg: &ContainerCfg) {
    let Ok(rd) = std::fs::read_dir(state_base()) else { return };
    for e in rd.flatten() {
        let name = e.file_name().to_string_lossy().into_owned();
        if name == own || !name.starts_with('r') { continue; }
        let Ok(l) = std::fs::File::open(e.path().join("lock")) else { continue };
        if unsafe { libc::flock(std::os::fd::AsRawFd::as_raw_fd(&l), libc::LOCK_EX | libc::LOCK_NB) } != 0 { continue; } // still alive
        for a in std::fs::read_dir(e.path()).into_iter().flatten().flatten() {
            let Ok(meta) = std::fs::read_to_string(a.path().join("meta.json")) else { continue };
            let m: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
            for k in ["pasta_pid", "supervisor_pid"] {
                if let Some(p) = m[k].as_i64().filter(|p| *p > 1) { unsafe { libc::kill(p as i32, libc::SIGKILL); } }
            }
            if let Some(id) = m["id"].as_str() {
                let mut c = std::process::Command::new(&cfg.runtime);
                c.args(&cfg.runtime_args).arg("--root").arg(e.path().join("crun"));
                let _ = c.args(["kill", id, "KILL"]).stdout(Stdio::null()).stderr(Stdio::null()).status();
                let mut c = std::process::Command::new(&cfg.runtime);
                c.args(&cfg.runtime_args).arg("--root").arg(e.path().join("crun"));
                let _ = c.args(["delete", "-f", id]).stdout(Stdio::null()).stderr(Stdio::null()).status();
            }
        }
        force_remove(&e.path());
        force_remove(&sock_base().join(&name));
    }
}

// ---------------------------------------------------------------- connection to the guest

struct Conn {
    tx: mpsc::UnboundedSender<Value>,
    pending: Mutex<HashMap<u64, mpsc::UnboundedSender<Value>>>,
    next: AtomicU64,
    dead: AtomicBool,
}

const MAX_REPLY: usize = 64 << 20;

impl Conn {
    fn start(stream: tokio::net::UnixStream) -> Arc<Conn> {
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let c = Arc::new(Conn { tx, pending: Default::default(), next: AtomicU64::new(1), dead: AtomicBool::new(false) });
        tokio::spawn(async move { while let Some(v) = rx.recv().await { if write_frame(&mut wr, &v).await.is_err() { break; } } });
        let c2 = c.clone();
        tokio::spawn(async move {
            // guest replies are untrusted data: bounded frames, validated shape, nothing interpreted
            while let Ok(ev) = read_frame(&mut rd).await {
                let Some(id) = ev["id"].as_u64() else { break };
                let kind = ev["ev"].as_str().unwrap_or("");
                if kind != "out" && kind != "done" { break; }
                let tx = { c2.pending.lock().unwrap().get(&id).cloned() };
                if let Some(tx) = tx {
                    if kind == "done" { c2.pending.lock().unwrap().remove(&id); }
                    let _ = tx.send(ev);
                }
            }
            c2.dead.store(true, Ordering::Relaxed);
            c2.pending.lock().unwrap().clear(); // receivers see the channel close
        });
        c
    }

    async fn call(&self, op: &str, args: Value) -> Result<(String, Value), String> {
        if self.dead.load(Ordering::Relaxed) { return Err("container connection lost".into()); }
        let id = self.next.fetch_add(1, Ordering::Relaxed);
        let (rtx, mut rrx) = mpsc::unbounded_channel();
        self.pending.lock().unwrap().insert(id, rtx);
        if self.tx.send(json!({"id": id, "op": op, "args": args})).is_err() { return Err("container connection lost".into()); }
        let mut out = String::new();
        loop {
            match rrx.recv().await {
                Some(ev) if ev["ev"] == "out" => {
                    out.push_str(ev["data"].as_str().unwrap_or(""));
                    if out.len() > MAX_REPLY { self.pending.lock().unwrap().remove(&id); return Err("guest reply too large".into()); }
                }
                Some(ev) => return Ok((out, ev)),
                None => return Err("container connection lost".into()),
            }
        }
    }
}

type Ready = Option<Result<Arc<Conn>, String>>;

struct Shared {
    ready: watch::Receiver<Ready>,
    cwd: Mutex<PathBuf>,
}

impl Shared {
    async fn conn(&self) -> Result<Arc<Conn>, String> {
        let mut rx = self.ready.clone();
        loop {
            if let Some(r) = &*rx.borrow() { return r.clone(); }
            if rx.changed().await.is_err() { return Err("container setup was abandoned".into()); }
        }
    }
    async fn call(&self, op: &str, args: Value) -> Result<(String, Value), String> { self.conn().await?.call(op, args).await }
}

struct RemoteFs(Arc<Shared>);

impl RemoteFs {
    async fn run(&self, op: &'static str, a: &Value) -> Result<String> {
        let (out, done) = self.0.call(op, a.clone()).await.map_err(|e| anyhow!("{e}"))?;
        if done["ok"] == true { Ok(out) } else { Err(anyhow!("{}", done["err"].as_str().unwrap_or("guest error"))) }
    }
}

impl FileOps for RemoteFs {
    fn read<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("read", a)) }
    fn write<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("write", a)) }
    fn edit<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("edit", a)) }
    fn multiedit<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("multiedit", a)) }
    fn glob<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("glob", a)) }
    fn grep<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("grep", a)) }
    fn ls<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>> { Box::pin(self.run("ls", a)) }
}

// ---------------------------------------------------------------- the sandbox

#[derive(Default)]
struct Procs {
    sup: Option<tokio::process::Child>,
    stdin: Option<tokio::process::ChildStdin>,
    pasta: Option<tokio::process::Child>,
}

pub struct ContainerSandbox {
    shared: Arc<Shared>,
    fs: RemoteFs,
    procs: Arc<tokio::sync::Mutex<Procs>>,
    dirs: Vec<PathBuf>,
    keep: bool,
    ports: Arc<Mutex<Vec<(u16, u16)>>>,
    spill: PathBuf,
}

/// Create the sandbox. Must be called inside a tokio runtime; the container starts in the background.
pub fn open(policy: Policy, opts: ContainerOpts) -> Result<ContainerSandbox, String> {
    let cfg = CFG.get().ok_or("tier 2 is not configured in this build")?.clone();
    // cheap synchronous prerequisites now (cached); heavy work happens in the background task
    let probe = probe::check(&cfg)?;
    if cfg.idmap == Idmap::Full {
        return Err("container.idmap \"full\" needs newuidmap/subuid support, which this build does not implement; use \"single\"".into());
    }
    if opts.net || !opts.ports.is_empty() { pick_net(&cfg)?; }
    let wants_limits = opts.mem.is_some() || opts.cpus.is_some() || opts.pids.is_some();
    if wants_limits && probe.cgroup.is_none() && cfg.require_limits {
        return Err("--require-limits: cgroup v2 delegation is not available for this user (memory/cpu/pids limits cannot be enforced)".into());
    }
    let image = opts.image.clone().unwrap_or_else(|| cfg.image.clone());
    let lower = rootfs_of(&image)?;
    let run = run_dir(&cfg)?;
    let agent = opts.agent.clone();
    let (state, sockdir) = (run.state.join(&agent), run.sock.join(&agent));
    mkdir_private(&state).map_err(|e| format!("{}: {e}", state.display()))?;
    mkdir_private(&sockdir).map_err(|e| format!("{}: {e}", sockdir.display()))?;
    let spill = sockdir.join("spill");
    let (rtx, rrx) = watch::channel::<Ready>(None);
    let shared = Arc::new(Shared { ready: rrx, cwd: Mutex::new(policy.cwd.clone()) });
    let procs = Arc::new(tokio::sync::Mutex::new(Procs::default()));
    let ports = Arc::new(Mutex::new(vec![]));
    let sb = ContainerSandbox { fs: RemoteFs(shared.clone()), shared, procs: procs.clone(), dirs: vec![state.clone(), sockdir.clone()], keep: cfg.keep_state, ports: ports.clone(), spill };
    let (run_id, probe2) = (run.id.clone(), probe.clone());
    tokio::spawn(async move {
        let r = setup(cfg, probe2, policy, opts, lower, state, sockdir, run_id, procs, ports).await;
        let _ = rtx.send(Some(r));
    });
    Ok(sb)
}

#[allow(clippy::too_many_arguments)]
async fn setup(cfg: ContainerCfg, probe: probe::Probe, policy: Policy, opts: ContainerOpts, lower: PathBuf, state: PathBuf, sockdir: PathBuf,
    run_id: String, procs: Arc<tokio::sync::Mutex<Procs>>, ports: Arc<Mutex<Vec<(u16, u16)>>>) -> Result<Arc<Conn>, String> {
    let agent = opts.agent.clone();
    let id = format!("zc-{run_id}-{agent}");
    let guest = tokio::task::spawn_blocking({ let g = cfg.guest; move || extract_guest(g) }).await.map_err(|e| e.to_string())??;
    // rootfs plan: the cheapest available provider (kernel overlayfs, fuse-overlayfs, copy)
    let (bundle, _) = (state.join("bundle"), ());
    std::fs::create_dir_all(&bundle).map_err(|e| e.to_string())?;
    let provider = rootfs::pick(&cfg.rootfs, &probe);
    let plan = provider.plan(&lower, &state);
    if let rootfs::Plan::Overlay { upper, work, merged, .. } | rootfs::Plan::Fuse { upper, work, merged, .. } = &plan {
        for d in [upper, work, merged] { std::fs::create_dir_all(d).map_err(|e| format!("{}: {e}", d.display()))?; }
    }
    { let p = plan_copy_job(&plan); if let Some(j) = p { tokio::task::spawn_blocking(j).await.map_err(|e| e.to_string())??; } }
    let rootfs_path = plan.rootfs().to_path_buf();
    // OCI bundle
    let cgroup_path = probe.cgroup.as_ref().map(|d| format!("{}/{id}", d.strip_prefix("/sys/fs/cgroup").unwrap_or(d).display()));
    let use_limits = probe.cgroup.is_some();
    let net_be = if opts.net || !opts.ports.is_empty() { Some(pick_net(&cfg)?) } else { None };
    // slirp4netns has no DNS forwarder on the host side: its resolver lives at 10.0.2.3
    let resolv = state.join("resolv.conf");
    if net_be == Some(Net::Slirp) { std::fs::write(&resolv, "nameserver 10.0.2.3\n").map_err(|e| e.to_string())?; }
    let spec = oci::Spec {
        resolv: (net_be == Some(Net::Slirp)).then_some(resolv.as_path()),
        rootfs: &rootfs_path, project: &policy.cwd, project_rw: opts.project_rw, write: &policy.write, guest: &guest, sockdir: &sockdir,
        agent: &agent, net: opts.net, mem: if use_limits { opts.mem } else { None }, cpus: if use_limits { opts.cpus } else { None },
        pids: if use_limits { opts.pids } else { None }, cgroup_path: if use_limits { cgroup_path } else { None }, seccomp: cfg.seccomp, readonly_rootfs: cfg.readonly_rootfs,
    };
    std::fs::write(bundle.join("config.json"), serde_json::to_vec_pretty(&oci::config(&spec)).unwrap()).map_err(|e| e.to_string())?;
    let mut runtime_args = cfg.runtime_args.clone();
    if use_limits && cfg.runtime_args == ["--cgroup-manager=disabled"] { runtime_args = vec![]; }
    // guest socket
    let sock_path = sockdir.join("g.sock");
    let _ = std::fs::remove_file(&sock_path);
    let listener = tokio::net::UnixListener::bind(&sock_path).map_err(|e| format!("{}: {e}", sock_path.display()))?;
    let _ = std::fs::set_permissions(&sock_path, std::fs::Permissions::from_mode(0o600));
    // supervisor
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    let spec_json = json!({
        "runtime": probe.runtime, "runtime_args": runtime_args, "root": state.parent().map(|p| p.join("crun")).unwrap_or_default(), "bundle": bundle, "id": id,
        "log": state.join("log"), "uid": uid, "gid": gid, "fuse": probe.fuse_overlayfs,
        "plan": plan.to_json(),
    });
    let root_dir = PathBuf::from(spec_json["root"].as_str().unwrap_or(""));
    std::fs::create_dir_all(&root_dir).map_err(|e| e.to_string())?;
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let mut child = tokio::process::Command::new(exe).arg("__ns").arg(spec_json.to_string())
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|e| format!("cannot start the namespace supervisor: {e}"))?;
    let sup_pid = child.id().unwrap_or(0);
    let stdin = child.stdin.take();
    let mut lines = tokio::io::BufReader::new(child.stdout.take().ok_or("no supervisor stdout")?).lines();
    let first = tokio::time::timeout(std::time::Duration::from_secs(60), lines.next_line()).await;
    {
        let mut p = procs.lock().await;
        p.sup = Some(child);
        p.stdin = stdin;
    }
    let meta = |pasta: i64| json!({"id": id, "supervisor_pid": sup_pid, "pasta_pid": pasta, "run": run_id});
    let _ = std::fs::write(state.join("meta.json"), meta(0).to_string());
    match first {
        Ok(Ok(Some(l))) if l.trim() == "ready" => {}
        Ok(Ok(Some(l))) => return Err(l.trim().trim_start_matches("error: ").to_string()),
        Ok(Ok(None)) => return Err("the namespace supervisor exited unexpectedly".into()),
        Ok(Err(e)) => return Err(e.to_string()),
        Err(_) => return Err("timed out creating the container".into()),
    }
    // network: slirp4netns attaches to the container's netns by pid
    if net_be == Some(Net::Slirp) {
        let pid = container_pid(&probe.runtime, &runtime_args, &root_dir, &id).await?;
        let api = state.join("slirp.sock"); // not under sockdir: the guest must never reach the control socket
        if api.as_os_str().len() >= 104 { return Err(format!("{} is too long for a unix socket; use a shorter XDG_STATE_HOME", api.display())); }
        let _ = std::fs::remove_file(&api);
        let slirp = tokio::process::Command::new(&cfg.slirp).args(["--configure", "--mtu=65520", "--disable-host-loopback", "-a"]).arg(&api).arg(pid.to_string()).arg("tap0")
            .stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn().map_err(|e| format!("cannot start slirp4netns: {e}"))?;
        let _ = std::fs::write(state.join("meta.json"), meta(slirp.id().unwrap_or(0) as i64).to_string());
        procs.lock().await.pasta = Some(slirp);
        let mut mapped = vec![];
        for g in &opts.ports { mapped.push((*g, alloc_port(cfg.port_range).ok_or("no free host port in the configured range")?)); }
        slirp_ready(&api, &mapped).await?;
        *ports.lock().unwrap() = mapped;
    } else if net_be == Some(Net::Pasta) {
        // pasta attaches the same way
        let pid = container_pid(&probe.runtime, &runtime_args, &root_dir, &id).await?;
        let mut cmd = tokio::process::Command::new(&cfg.pasta);
        cmd.args(["-f", "-q", "--config-net"]);
        let mut mapped = vec![];
        for g in &opts.ports {
            let h = alloc_port(cfg.port_range).ok_or("no free host port in the configured range")?;
            cmd.arg("-t").arg(format!("{h}:{g}"));
            mapped.push((*g, h));
        }
        let pasta = cmd.arg(pid.to_string()).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).kill_on_drop(true).spawn()
            .map_err(|e| format!("cannot start pasta: {e}"))?;
        let _ = std::fs::write(state.join("meta.json"), meta(pasta.id().unwrap_or(0) as i64).to_string());
        procs.lock().await.pasta = Some(pasta);
        *ports.lock().unwrap() = mapped;
    }
    // the guest connects out; only this agent's socket dir is visible to it
    let (stream, _) = tokio::time::timeout(std::time::Duration::from_secs(30), listener.accept()).await
        .map_err(|_| "timed out waiting for the guest to connect".to_string())?.map_err(|e| e.to_string())?;
    match stream.peer_cred() {
        Ok(c) if c.uid() == uid => {}
        _ => return Err("guest connection rejected: unexpected peer credentials".into()),
    }
    let conn = Conn::start(stream);
    let (out, done) = conn.call("ping", json!({})).await?;
    if done["ok"] != true || out != "pong" { return Err("guest handshake failed".into()); }
    Ok(conn)
}

/// blocking host-side preparation (only the copy provider has any)
fn plan_copy_job(plan: &rootfs::Plan) -> Option<impl FnOnce() -> Result<(), String> + Send + 'static> {
    match plan {
        rootfs::Plan::Copy { lower, rootfs } => { let (l, r) = (lower.clone(), rootfs.clone()); Some(move || rootfs::Plan::Copy { lower: l, rootfs: r }.prepare_host()) }
        _ => None,
    }
}

async fn container_pid(rt: &Path, args: &[String], root: &Path, id: &str) -> Result<i64, String> {
    let o = tokio::process::Command::new(rt).args(args).arg("--root").arg(root).args(["state", id]).stdin(Stdio::null()).output().await.map_err(|e| e.to_string())?;
    let v: Value = serde_json::from_slice(&o.stdout).map_err(|_| "cannot read container state".to_string())?;
    v["pid"].as_i64().filter(|p| *p > 0).ok_or_else(|| "container has no pid".into())
}

/// How a container with `net` or `ports` is attached to the outside.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Net { Pasta, Slirp }

/// Fail closed: a missing backend is an error, never a silent downgrade to an empty netns.
fn pick_net(cfg: &ContainerCfg) -> Result<Net, String> {
    let (pasta, slirp) = (probe::which(&cfg.pasta).is_some(), probe::which(&cfg.slirp).is_some());
    match cfg.net_backend.as_str() {
        "pasta" if pasta => Ok(Net::Pasta),
        "slirp4netns" if slirp => Ok(Net::Slirp),
        "auto" if pasta => Ok(Net::Pasta),
        "auto" if slirp => Ok(Net::Slirp),
        "pasta" => Err(format!("net=true needs `{}` (passt) to attach the container to the network; install passt, or use net=false", cfg.pasta)),
        "slirp4netns" => Err(format!("container.net_backend is slirp4netns but `{}` was not found; install slirp4netns, or use net=false", cfg.slirp)),
        _ => Err(format!("net=true needs `{}` (passt) or `{}` (slirp4netns) to attach the container to the network; install one, or use net=false", cfg.pasta, cfg.slirp)),
    }
}

/// Wait for slirp4netns' API socket (created once the tap is configured) and install the TCP port forwards.
async fn slirp_ready(sock: &Path, fwd: &[(u16, u16)]) -> Result<(), String> {
    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
    let t = std::time::Instant::now();
    let st = loop {
        match tokio::net::UnixStream::connect(sock).await {
            Ok(s) => break s,
            Err(e) if t.elapsed() > std::time::Duration::from_secs(5) => return Err(format!("slirp4netns did not come up: {e}")),
            Err(_) => tokio::time::sleep(std::time::Duration::from_millis(20)).await,
        }
    };
    let (r, mut w) = st.into_split();
    let mut lines = tokio::io::BufReader::new(r).lines();
    for (guest, host) in fwd {
        let req = json!({"execute": "add_hostfwd", "arguments": {"proto": "tcp", "host_addr": "127.0.0.1", "host_port": host, "guest_port": guest}});
        w.write_all(format!("{req}\n").as_bytes()).await.map_err(|e| e.to_string())?;
        let l = tokio::time::timeout(std::time::Duration::from_secs(5), lines.next_line()).await
            .map_err(|_| "slirp4netns did not answer".to_string())?.map_err(|e| e.to_string())?.unwrap_or_default();
        if l.contains("\"error\"") { return Err(format!("slirp4netns refused the port forward {host} -> {guest}: {l}")); }
    }
    Ok(())
}

fn alloc_port((lo, hi): (u16, u16)) -> Option<u16> {
    let span = (hi.saturating_sub(lo)) as u32 + 1;
    let start = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0) % span;
    (0..span).map(|i| lo + ((start + i) % span) as u16).find(|p| std::net::TcpListener::bind(("127.0.0.1", *p)).is_ok())
}

/// Extract the embedded guest once to `~/.cache/zcode/guest-<version>` (size + hash checked), read-only.
fn extract_guest(bytes: &'static [u8]) -> Result<PathBuf, String> { extract_into(&crate::local::cache_dir(), bytes) }

fn extract_into(dir: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    let h = crate::fsops::hash(bytes);
    let p = dir.join(format!("guest-{}-{:016x}", env!("CARGO_PKG_VERSION"), h));
    let ok = std::fs::read(&p).map(|b| b.len() == bytes.len() && crate::fsops::hash(&b) == h).unwrap_or(false);
    if !ok {
        std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| e.to_string())?;
        // two containers starting together in one process must not share a temp file (the pid alone is not unique)
        static N: AtomicU64 = AtomicU64::new(0);
        let tmp = p.with_extension(format!("tmp{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        std::fs::write(&tmp, bytes).map_err(|e| e.to_string())?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o555)).map_err(|e| e.to_string())?;
        if let Err(e) = std::fs::rename(&tmp, &p) {
            let _ = std::fs::remove_file(&tmp);
            // another start finished first: fine as long as the file is complete
            if !std::fs::read(&p).is_ok_and(|b| b.len() == bytes.len() && crate::fsops::hash(&b) == h) { return Err(e.to_string()); }
        }
    }
    Ok(p)
}

impl Sandbox for ContainerSandbox {
    fn exec(&self, req: ExecReq) -> BoxFut<'_, Result<ExecStream>> {
        Box::pin(async move {
            let (op, args) = match &req {
                ExecReq::Run { command, timeout, background } => ("exec", json!({"command": command, "timeout": timeout.as_secs(), "background": background})),
                ExecReq::JobOutput { id, lines } => ("bash_output", json!({"id": id, "lines": lines})),
                ExecReq::JobKill { id } => ("bash_kill", json!({"id": id})),
            };
            let (out, done) = self.shared.call(op, args).await.map_err(|e| anyhow!("{e}"))?;
            if done["ok"] != true { return Err(anyhow!("{}", done["err"].as_str().unwrap_or("guest error"))); }
            if let Some(c) = done["cwd"].as_str() { *self.shared.cwd.lock().unwrap() = PathBuf::from(c); }
            let code = done["code"].as_i64().map(|c| c as i32);
            Ok(ExecStream::ready(out.into_bytes(), ExecDone { code, timed_out: done["timed_out"].as_bool().unwrap_or(false) }))
        })
    }
    fn fs(&self) -> &dyn FileOps { &self.fs }
    fn close(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if let Ok(c) = self.shared.conn().await { let _ = tokio::time::timeout(std::time::Duration::from_secs(1), c.call("shutdown", json!({}))).await; }
            let (sup, stdin, pasta) = { let mut p = self.procs.lock().await; (p.sup.take(), p.stdin.take(), p.pasta.take()) };
            drop(stdin); // EOF: the supervisor kills and deletes the container
            if let Some(mut p) = pasta { let _ = p.start_kill(); let _ = p.wait().await; }
            if let Some(mut s) = sup && tokio::time::timeout(std::time::Duration::from_secs(8), s.wait()).await.is_err() {
                let _ = s.start_kill();
                let _ = s.wait().await;
            }
            if !self.keep { for d in &self.dirs { force_remove(d); } }
        })
    }
    fn cwd(&self) -> PathBuf { self.shared.cwd.lock().unwrap().clone() }
    fn tier(&self) -> Tier { Tier::Container }
    fn ready(&self) -> BoxFut<'_, Result<()>> { Box::pin(async move { self.shared.conn().await.map(|_| ()).map_err(|e| anyhow!("{e}")) }) }
    fn ports(&self) -> Vec<(u16, u16)> { self.ports.lock().unwrap().clone() }
    fn spill_dir(&self) -> Option<(PathBuf, String)> { Some((self.spill.clone(), "/run/zcode/spill".into())) }
}

#[cfg(test)]
mod tests;
