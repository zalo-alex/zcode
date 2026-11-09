//! The `Sandbox` abstraction every tool goes through, plus `Policy` and the tier factory.
use anyhow::{Result, bail};
use serde_json::Value;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Clone, Copy, PartialEq, Eq, Debug, PartialOrd, Ord)]
pub enum Tier {
    /// no isolation (permission prompts only)
    None = 0,
    /// Landlock + seccomp on the bash child, path-checked file tools
    Landlock = 1,
    /// one rootless OCI container per agent
    Container = 2,
}
impl Tier {
    pub fn n(self) -> u8 { self as u8 }
    pub fn name(self) -> &'static str {
        match self { Tier::None => "none", Tier::Landlock => "landlock", Tier::Container => "container" }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ResLimits {
    pub nofile: Option<u64>,
    pub core: Option<u64>,
}

/// Per-agent container options (from a profile); only used by tier 2.
#[derive(Clone, Debug, Default)]
pub struct ContainerOpts {
    pub agent: String,
    pub image: Option<String>,
    pub net: bool,
    pub mem: Option<u64>,
    pub cpus: Option<f64>,
    pub pids: Option<u64>,
    pub ports: Vec<u16>,
    /// the lead owns the whole project read-write
    pub project_rw: bool,
}

/// What one agent may touch.
#[derive(Clone, Debug)]
pub struct Policy {
    /// project root; initial working directory
    pub cwd: PathBuf,
    /// extra read-only roots (on top of the system and toolchain defaults)
    pub read: Vec<PathBuf>,
    /// read-write roots (the project root for the lead)
    pub write: Vec<PathBuf>,
    /// outbound network (AF_INET/AF_INET6, `webfetch`)
    pub net: bool,
    /// private temp dir; created on demand when `None` and the tier is enforced
    pub tmp: Option<PathBuf>,
    pub limits: ResLimits,
    /// extra environment variable names passed to the bash child
    pub env: Vec<String>,
    /// read-only toolchain/cache dirs (`~/.cargo`, ...)
    pub toolchain: Vec<PathBuf>,
    /// `--allow-outside` / `--auto`: file tools may reach anywhere except the deny list. bash is NOT widened.
    pub outside: bool,
    /// grant read access to all of /proc (needed by some tools; exposes same-uid process info)
    pub proc: bool,
    /// run bash with a scrubbed environment even at tier 0 (daemon instances never inherit the daemon env)
    pub scrub_env: bool,
}

pub fn home() -> PathBuf { PathBuf::from(std::env::var("HOME").unwrap_or_default()) }

impl Policy {
    pub fn new(cwd: PathBuf) -> Self {
        let h = home();
        let toolchain = [".cargo", ".rustup", ".cache", ".npm"].iter().map(|d| h.join(d)).collect();
        Policy { write: vec![cwd.clone()], cwd, read: vec![], net: false, tmp: None, limits: ResLimits::default(),
            env: vec![], toolchain, outside: false, proc: false, scrub_env: false }
    }
    /// Always denied, for bash and file tools (never overridable).
    pub fn hard_deny() -> Vec<PathBuf> {
        let h = home();
        let xdg = std::env::var("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|_| h.join(".config"));
        let mut v = vec![xdg.join("zcode"), h.join(".config/zcode"), h.join(".config/opencode"), h.join(".ssh")];
        v.dedup();
        v
    }
}

#[derive(Clone, Debug)]
pub enum ExecReq {
    /// run a shell command; the working directory persists between calls
    Run { command: String, timeout: Duration, background: bool },
    JobOutput { id: Option<String>, lines: usize },
    JobKill { id: String },
}

#[derive(Debug, Clone)]
pub struct ExecDone {
    pub code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Debug)]
pub enum ExecEvent {
    Out(Vec<u8>),
    Done(ExecDone),
}

pub struct ExecStream {
    pub rx: tokio::sync::mpsc::Receiver<ExecEvent>,
}
impl ExecStream {
    /// A stream that already holds the whole result.
    pub fn ready(out: Vec<u8>, done: ExecDone) -> Self {
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        let _ = tx.try_send(ExecEvent::Out(out));
        let _ = tx.try_send(ExecEvent::Done(done));
        ExecStream { rx }
    }
    pub async fn collect(mut self) -> (String, ExecDone) {
        let mut buf = Vec::new();
        let mut done = ExecDone { code: None, timed_out: false };
        while let Some(ev) = self.rx.recv().await {
            match ev {
                ExecEvent::Out(b) => buf.extend_from_slice(&b),
                ExecEvent::Done(d) => { done = d; break; }
            }
        }
        (String::from_utf8_lossy(&buf).into_owned(), done)
    }
}

/// File operations. Arguments are the tool-call JSON, results are the tool output text, so the same
/// contract can be forwarded to a guest process unchanged.
pub trait FileOps: Send + Sync {
    fn read<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
    fn write<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
    fn edit<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
    fn multiedit<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
    fn glob<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
    fn grep<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
    fn ls<'a>(&'a self, a: &'a Value) -> BoxFut<'a, Result<String>>;
}

pub trait Sandbox: Send + Sync {
    /// bash, including background jobs and kill
    fn exec(&self, req: ExecReq) -> BoxFut<'_, Result<ExecStream>>;
    fn fs(&self) -> &dyn FileOps;
    fn close(&self) -> BoxFut<'_, ()>;
    /// current working directory of the persistent shell (as seen by the file tools)
    fn cwd(&self) -> PathBuf;
    fn tier(&self) -> Tier;
    /// resolves once the sandbox can run commands (tier 2 creates its container in the background)
    fn ready(&self) -> BoxFut<'_, Result<()>> { Box::pin(async { Ok(()) }) }
    /// forwarded ports (guest, host)
    fn ports(&self) -> Vec<(u16, u16)> { vec![] }
    /// (host dir, path the sandbox sees) where oversized tool output may be spilled
    fn spill_dir(&self) -> Option<(PathBuf, String)> { None }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    None,
    Landlock,
    /// tier 1 when the kernel supports it, otherwise run unconfined with one warning
    Warn,
    Container,
}

/// Build the sandbox for `mode`. Fails closed: an unenforceable request is an error, never a silent downgrade.
/// The returned string is a one-per-session warning for the caller to print.
pub fn open(mode: Mode, policy: Policy, auto: bool, require: bool) -> Result<(Arc<dyn Sandbox>, Option<String>)> {
    open_with(mode, policy, auto, require, ContainerOpts { agent: "lead".into(), project_rw: true, ..Default::default() })
}

pub fn open_with(mode: Mode, policy: Policy, auto: bool, require: bool, copts: ContainerOpts) -> Result<(Arc<dyn Sandbox>, Option<String>)> {
    let _ = &copts;
    match mode {
        Mode::None => {
            if require { bail!("--require-sandbox: sandbox is disabled (--sandbox none)"); }
            Ok((Arc::new(crate::local::LocalSandbox::open_none(policy)), None))
        }
        Mode::Landlock | Mode::Warn => match crate::local::LocalSandbox::open_tier1(policy.clone()) {
            Ok(s) => Ok((Arc::new(s), None)),
            Err(why) => {
                let tolerant = mode == Mode::Warn || (!auto && !require);
                if !tolerant {
                    bail!("tier 1 sandbox (Landlock + seccomp) cannot be enforced: {why}.\n  Use `--sandbox warn` to continue unconfined with a warning, `--sandbox none` to opt out, or `--sandbox container`.");
                }
                if require { bail!("--require-sandbox: tier 1 sandbox cannot be enforced: {why}"); }
                let mut p = policy;
                p.net = true;
                Ok((Arc::new(crate::local::LocalSandbox::open_none(p)),
                    Some(format!("warning: tier 1 sandbox unavailable ({why}); running WITHOUT confinement"))))
            }
        },
        #[cfg(all(feature = "host", target_os = "linux"))]
        Mode::Container => {
            let mut p = policy;
            p.net = copts.net;
            let sb = crate::container::open(p, copts).map_err(|e| anyhow::anyhow!("tier 2 sandbox cannot be enforced: {e}"))?;
            Ok((Arc::new(sb), None))
        }
        #[cfg(not(all(feature = "host", target_os = "linux")))]
        Mode::Container => bail!("tier 2 (--sandbox container) is not available in this build"),
    }
}
