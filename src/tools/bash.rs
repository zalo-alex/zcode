//! bash tool: foreground (output via temp file so backgrounded children neither hang nor get SIGPIPE),
//! persistent cwd, and detached background jobs with log files.
use super::{Ctx, s};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Mutex, OnceLock, atomic::{AtomicU64, Ordering}};
use std::time::Duration;
use tokio::process::Command;

unsafe extern "C" { fn kill(pid: i32, sig: i32) -> i32; }
fn kill_group(pid: u32, sig: i32) { unsafe { kill(-(pid as i32), sig); } }

static COUNTER: AtomicU64 = AtomicU64::new(1);

struct Job { pid: u32, log: PathBuf, exit: PathBuf, cmd: String }
fn jobs() -> &'static Mutex<HashMap<String, Job>> {
    static J: OnceLock<Mutex<HashMap<String, Job>>> = OnceLock::new();
    J.get_or_init(Default::default)
}

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

pub async fn bash(a: &Value, ctx: &Ctx) -> Result<String> {
    let cmd = s(a, "command")?;
    if a["background"].as_bool().unwrap_or(false) {
        return background(cmd, ctx).await;
    }
    let secs = a["timeout"].as_u64().unwrap_or_else(default_timeout).clamp(1, 3600);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir();
    let out_f = tmp.join(format!("zcode-{}-{n}.out", std::process::id()));
    let cwd_f = tmp.join(format!("zcode-{}-{n}.cwd", std::process::id()));
    let script = format!("{cmd}\n__zc=$?\npwd -P > {} 2>/dev/null\nexit $__zc\n", quote(&cwd_f));
    let out = std::fs::File::create(&out_f)?;
    let err = out.try_clone()?;
    let mut child = Command::new("sh").arg("-c").arg(script).current_dir(ctx.cwd())
        .stdin(Stdio::null()).stdout(out).stderr(err)
        .kill_on_drop(true).process_group(0).spawn()?;
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
    let mut text = read_head_tail(&out_f, 24_000);
    if let Ok(d) = std::fs::read_to_string(&cwd_f) {
        let d = PathBuf::from(d.trim());
        if d.is_dir() { *ctx.cwd.lock().unwrap() = d; }
    }
    let _ = std::fs::remove_file(&out_f);
    let _ = std::fs::remove_file(&cwd_f);
    if timed_out {
        text.push_str(&format!("\n[timeout after {secs}s, killed. For long-running or never-ending commands use background: true]"));
    } else if status != 0 {
        text.push_str(&format!("\n[exit {status}]"));
    }
    Ok(if text.trim().is_empty() { "(no output)".into() } else { text })
}

async fn background(cmd: &str, ctx: &Ctx) -> Result<String> {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = cache_dir().join("jobs");
    std::fs::create_dir_all(&dir)?;
    let base = format!("{}-{n}", std::process::id());
    let (log, exit) = (dir.join(format!("{base}.log")), dir.join(format!("{base}.exit")));
    let script = format!("(\n{cmd}\n)\necho $? > {}\n", quote(&exit));
    let out = std::fs::File::create(&log)?;
    let err = out.try_clone()?;
    let child = Command::new("sh").arg("-c").arg(script).current_dir(ctx.cwd())
        .stdin(Stdio::null()).stdout(out).stderr(err).process_group(0).spawn()?;
    let pid = child.id().ok_or_else(|| anyhow!("failed to start"))?;
    let id = format!("job{n}");
    jobs().lock().unwrap().insert(id.clone(), Job { pid, log: log.clone(), exit, cmd: cmd.to_string() });
    tokio::time::sleep(Duration::from_millis(700)).await;
    Ok(format!("started background job {id} (pid {pid}), log {}\n--- output so far ---\n{}", log.display(), tail_lines(&log, 30)))
}

fn status_of(j: &Job) -> String {
    match std::fs::read_to_string(&j.exit) {
        Ok(c) => format!("exited with code {}", c.trim()),
        Err(_) => "running".into(),
    }
}

pub fn output(a: &Value) -> Result<String> {
    let map = jobs().lock().unwrap();
    let Some(id) = a["id"].as_str() else {
        if map.is_empty() { return Ok("no background jobs".into()); }
        let mut v: Vec<_> = map.iter().collect();
        v.sort_by_key(|(k, _)| k.trim_start_matches("job").parse::<u64>().unwrap_or(0));
        return Ok(v.iter().map(|(k, j)| format!("{k}: {} (pid {}) `{}`", status_of(j), j.pid, j.cmd.lines().next().unwrap_or(""))).collect::<Vec<_>>().join("\n"));
    };
    let j = map.get(id).ok_or_else(|| anyhow!("no such job `{id}`"))?;
    let n = a["lines"].as_u64().unwrap_or(100) as usize;
    Ok(format!("{id}: {}\n{}", status_of(j), tail_lines(&j.log, n)))
}

pub async fn kill_job(a: &Value) -> Result<String> {
    let id = s(a, "id")?;
    let pid = jobs().lock().unwrap().get(id).map(|j| j.pid).ok_or_else(|| anyhow!("no such job `{id}`"))?;
    kill_group(pid, 15);
    tokio::time::sleep(Duration::from_millis(300)).await;
    kill_group(pid, 9);
    Ok(format!("{id} killed"))
}
