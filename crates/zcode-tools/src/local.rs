//! Local sandbox (tier 0 and tier 1). Tier 0 = the original behavior; tier 1 adds Landlock + seccomp on
//! the bash child, a scrubbed environment, a private tmp dir and a path-checked `LocalFs`.
use crate::fsops::LocalFs;
use crate::sandbox::*;
use anyhow::{Result, anyhow};
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::process::Command;

unsafe extern "C" { fn kill(pid: i32, sig: i32) -> i32; }
fn kill_group(pid: u32, sig: i32) { unsafe { kill(-(pid as i32), sig); } }

struct Job { pid: u32, log: PathBuf, exit: PathBuf, cmd: String }

pub fn cache_dir() -> PathBuf {
    std::env::var("XDG_CACHE_HOME").map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".cache")).join("zcode")
}

fn default_timeout() -> u64 {
    std::env::var("ZCODE_BASH_TIMEOUT").ok().and_then(|s| s.parse().ok()).unwrap_or(120)
}

/// Read a file; when large keep the first and last `half` bytes.
fn read_head_tail(p: &Path, half: u64) -> String {
    let Ok(mut f) = std::fs::File::open(p) else { return String::new() };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let mut buf = Vec::new();
    if len <= half * 2 {
        let _ = f.read_to_end(&mut buf);
        return String::from_utf8_lossy(&buf).into_owned();
    }
    let mut head = vec![0u8; half as usize];
    let _ = f.read_exact(&mut head);
    let mut tail = vec![0u8; half as usize];
    let _ = f.seek(SeekFrom::End(-(half as i64)));
    let _ = f.read_exact(&mut tail);
    format!("{}\n... [{} bytes omitted] ...\n{}", String::from_utf8_lossy(&head), len - half * 2, String::from_utf8_lossy(&tail))
}

fn tail_lines(p: &Path, n: usize) -> String {
    let t = read_head_tail(p, 64_000);
    let lines: Vec<&str> = t.lines().collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

fn quote(p: &Path) -> String { format!("'{}'", p.display().to_string().replace('\'', "'\\''")) }

pub struct LocalSandbox {
    tier: Tier,
    policy: Policy,
    cwd: Arc<Mutex<PathBuf>>,
    fs: LocalFs,
    tmp: PathBuf,
    private_tmp: bool,
    jobs_dir: PathBuf,
    jobs: Mutex<HashMap<String, Job>>,
    #[cfg(all(target_os = "linux", feature = "tier1"))]
    prep: Option<Lazy>,
}

/// Tier 1 confinement data. Probing the kernel and validating the policy happen at open (fail closed, cheap); opening the
/// rule fds and compiling the seccomp filter wait for the first `bash`, so an idle session never pays for them.
#[cfg(all(target_os = "linux", feature = "tier1"))]
struct Lazy {
    caps: crate::tier1::Caps,
    cell: std::sync::OnceLock<std::result::Result<Arc<crate::tier1::Prepared>, String>>,
}

static TMP_N: AtomicU64 = AtomicU64::new(0);
static COUNTER: AtomicU64 = AtomicU64::new(1);

fn private_tmp() -> std::io::Result<PathBuf> {
    use std::os::unix::fs::DirBuilderExt;
    let uid = unsafe { libc::getuid() };
    let base = match std::env::var("XDG_RUNTIME_DIR") {
        Ok(d) if !d.is_empty() && Path::new(&d).is_dir() => PathBuf::from(d).join("zcode"),
        _ => PathBuf::from(format!("/tmp/zcode-{uid}")),
    };
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true).mode(0o700);
    b.create(&base)?;
    let d = base.join(format!("t{}-{}", std::process::id(), TMP_N.fetch_add(1, Ordering::Relaxed)));
    let mut b = std::fs::DirBuilder::new();
    b.mode(0o700);
    b.create(&d)?;
    Ok(d)
}

impl LocalSandbox {
    /// Tier 0: no isolation.
    pub fn open_none(mut policy: Policy) -> Self {
        policy.net = true;
        let tmp = policy.tmp.clone().unwrap_or_else(std::env::temp_dir);
        let cwd = Arc::new(Mutex::new(policy.cwd.clone()));
        LocalSandbox {
            tier: Tier::None, fs: LocalFs::new(None, cwd.clone()), cwd, tmp, private_tmp: false,
            jobs_dir: cache_dir().join("jobs"), jobs: Default::default(), policy,
            #[cfg(all(target_os = "linux", feature = "tier1"))] prep: None,
        }
    }

    /// Tier 1: Landlock + seccomp. Err = reason it cannot be enforced.
    #[cfg(all(target_os = "linux", feature = "tier1"))]
    pub fn open_tier1(policy: Policy) -> std::result::Result<Self, String> {
        let caps = crate::tier1::probe()?;
        let tmp = match &policy.tmp { Some(t) => t.clone(), None => private_tmp().map_err(|e| format!("cannot create private tmp dir: {e}"))? };
        crate::tier1::Prepared::validate(&policy, &tmp)?;
        // keep /proc/<zcode>/environ out of reach of same-uid readers
        unsafe { libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0); }
        let guard = crate::pathguard::PathGuard::new(&policy, &tmp);
        let cwd = Arc::new(Mutex::new(policy.cwd.clone()));
        Ok(LocalSandbox {
            tier: Tier::Landlock, fs: LocalFs::new(Some(guard), cwd.clone()), cwd, jobs_dir: tmp.join("jobs"), tmp,
            private_tmp: policy.tmp.is_none(), jobs: Default::default(), policy,
            prep: Some(Lazy { caps, cell: Default::default() }),
        })
    }

    #[cfg(not(all(target_os = "linux", feature = "tier1")))]
    pub fn open_tier1(_policy: Policy) -> std::result::Result<Self, String> { Err("Landlock requires Linux".into()) }

    pub fn landlock_abi(&self) -> Option<u32> {
        #[cfg(all(target_os = "linux", feature = "tier1"))]
        { self.prep.as_ref().map(|l| l.caps.abi.min(6)) }
        #[cfg(not(all(target_os = "linux", feature = "tier1")))]
        { None }
    }

    fn command(&self, script: String) -> Result<Command> {
        let mut c = Command::new("sh");
        c.arg("-c").arg(script).current_dir(self.cwd.lock().unwrap().clone()).stdin(Stdio::null());
        #[cfg(all(target_os = "linux", feature = "tier1"))]
        if self.prep.is_none() && self.policy.scrub_env {
            c.env_clear();
            for (k, v) in crate::tier1::child_env(&self.policy, &self.tmp) { c.env(k, v); }
        }
        #[cfg(all(target_os = "linux", feature = "tier1"))]
        if let Some(l) = &self.prep {
            let prep = l.cell.get_or_init(|| crate::tier1::Prepared::build(&self.policy, &self.tmp, &l.caps).map(Arc::new)).clone()
                .map_err(|e| anyhow!("tier 1 sandbox cannot be enforced: {e}"))?;
            c.env_clear();
            for (k, v) in crate::tier1::child_env(&self.policy, &self.tmp) { c.env(k, v); }
            // SAFETY: confine_child only issues raw syscalls on data built before the fork.
            unsafe { c.pre_exec(move || prep.confine_child()); }
        }
        Ok(c)
    }

    async fn run(&self, command: &str, secs: u64) -> Result<ExecStream> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let out_f = self.tmp.join(format!("zcode-{}-{n}.out", std::process::id()));
        let cwd_f = self.tmp.join(format!("zcode-{}-{n}.cwd", std::process::id()));
        let script = format!("{command}\n__zc=$?\npwd -P > {} 2>/dev/null\nexit $__zc\n", quote(&cwd_f));
        let out = std::fs::File::create(&out_f)?;
        let err = out.try_clone()?;
        let mut child = self.command(script)?.stdout(out).stderr(err).kill_on_drop(true).process_group(0).spawn()
            .map_err(|e| anyhow!("failed to start shell: {e}"))?;
        let pid = child.id();
        let mut timed_out = false;
        let status = match tokio::time::timeout(Duration::from_secs(secs), child.wait()).await {
            Ok(st) => st?.code().unwrap_or(-1),
            Err(_) => {
                timed_out = true;
                if let Some(p) = pid { kill_group(p, 9); }
                let _ = child.wait().await;
                -1
            }
        };
        let text = read_head_tail(&out_f, 24_000);
        if let Ok(d) = std::fs::read_to_string(&cwd_f) {
            let d = PathBuf::from(d.trim());
            if d.is_dir() { *self.cwd.lock().unwrap() = d; }
        }
        let _ = std::fs::remove_file(&out_f);
        let _ = std::fs::remove_file(&cwd_f);
        Ok(ExecStream::ready(text.into_bytes(), ExecDone { code: Some(status), timed_out }))
    }

    async fn background(&self, cmd: &str) -> Result<ExecStream> {
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::fs::create_dir_all(&self.jobs_dir)?;
        let base = format!("{}-{n}", std::process::id());
        let (log, exit) = (self.jobs_dir.join(format!("{base}.log")), self.jobs_dir.join(format!("{base}.exit")));
        let script = format!("(\n{cmd}\n)\necho $? > {}\n", quote(&exit));
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let child = self.command(script)?.stdout(out).stderr(err).process_group(0).spawn()
            .map_err(|e| anyhow!("failed to start shell: {e}"))?;
        let pid = child.id().ok_or_else(|| anyhow!("failed to start"))?;
        let id = format!("job{n}");
        self.jobs.lock().unwrap().insert(id.clone(), Job { pid, log: log.clone(), exit, cmd: cmd.to_string() });
        tokio::time::sleep(Duration::from_millis(700)).await;
        let t = format!("started background job {id} (pid {pid}), log {}\n--- output so far ---\n{}", log.display(), tail_lines(&log, 30));
        Ok(ExecStream::ready(t.into_bytes(), ExecDone { code: None, timed_out: false }))
    }

    fn status_of(j: &Job) -> String {
        match std::fs::read_to_string(&j.exit) {
            Ok(c) => format!("exited with code {}", c.trim()),
            Err(_) => "running".into(),
        }
    }

    fn job_output(&self, id: Option<&str>, n: usize) -> Result<String> {
        let map = self.jobs.lock().unwrap();
        let Some(id) = id else {
            if map.is_empty() { return Ok("no background jobs".into()); }
            let mut v: Vec<_> = map.iter().collect();
            v.sort_by_key(|(k, _)| k.trim_start_matches("job").parse::<u64>().unwrap_or(0));
            return Ok(v.iter().map(|(k, j)| format!("{k}: {} (pid {}) `{}`", Self::status_of(j), j.pid, j.cmd.lines().next().unwrap_or(""))).collect::<Vec<_>>().join("\n"));
        };
        let j = map.get(id).ok_or_else(|| anyhow!("no such job `{id}`"))?;
        Ok(format!("{id}: {}\n{}", Self::status_of(j), tail_lines(&j.log, n)))
    }

    async fn job_kill(&self, id: &str) -> Result<String> {
        let pid = self.jobs.lock().unwrap().get(id).map(|j| j.pid).ok_or_else(|| anyhow!("no such job `{id}`"))?;
        kill_group(pid, 15);
        tokio::time::sleep(Duration::from_millis(300)).await;
        kill_group(pid, 9);
        Ok(format!("{id} killed"))
    }
}

fn text_stream(s: String) -> ExecStream { ExecStream::ready(s.into_bytes(), ExecDone { code: None, timed_out: false }) }

impl Sandbox for LocalSandbox {
    fn exec(&self, req: ExecReq) -> BoxFut<'_, Result<ExecStream>> {
        Box::pin(async move {
            match req {
                ExecReq::Run { command, timeout, background } => {
                    if background { self.background(&command).await } else { self.run(&command, timeout.as_secs().clamp(1, 3600)).await }
                }
                ExecReq::JobOutput { id, lines } => Ok(text_stream(self.job_output(id.as_deref(), lines)?)),
                ExecReq::JobKill { id } => Ok(text_stream(self.job_kill(&id).await?)),
            }
        })
    }
    fn fs(&self) -> &dyn FileOps { &self.fs }
    fn close(&self) -> BoxFut<'_, ()> {
        Box::pin(async move {
            if self.tier != Tier::None {
                let pids: Vec<u32> = self.jobs.lock().unwrap().values().map(|j| j.pid).collect();
                for p in pids { kill_group(p, 9); }
            }
            if self.private_tmp { let _ = std::fs::remove_dir_all(&self.tmp); }
        })
    }
    fn cwd(&self) -> PathBuf { self.cwd.lock().unwrap().clone() }
    fn tier(&self) -> Tier { self.tier }
}

pub fn default_timeout_secs() -> u64 { default_timeout() }
