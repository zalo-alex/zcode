//! Lazy, cached prerequisite checks for tier 2. Everything required fails closed with an actionable message.
use super::ContainerCfg;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Debug)]
pub struct Probe {
    pub runtime: PathBuf,
    pub overlay: bool,
    pub fuse_overlayfs: Option<PathBuf>,
    pub cgroup: Option<PathBuf>,
}

pub fn which(name: &str) -> Option<PathBuf> {
    let p = Path::new(name);
    if p.components().count() > 1 { return p.is_file().then(|| p.to_path_buf()); }
    std::env::var("PATH").ok()?.split(':').map(|d| Path::new(d).join(name)).find(|c| c.is_file())
}

/// A cgroup v2 directory we may create children in (delegation), if any.
///
/// The kernel refuses to enable domain controllers (`memory`) for children while the cgroup itself holds processes ("no internal
/// processes"). When a needed controller is not enabled yet we therefore move this process into a leaf child (`zcode-main`) first
/// and then enable the controllers; containers are created as siblings of that leaf.
pub fn delegated_cgroup() -> Option<PathBuf> {
    let s = std::fs::read_to_string("/proc/self/cgroup").ok()?;
    let rel = s.lines().find_map(|l| l.strip_prefix("0::"))?.trim().trim_start_matches('/').to_string();
    let dir = Path::new("/sys/fs/cgroup").join(&rel);
    let ctl = std::fs::read_to_string(dir.join("cgroup.controllers")).ok()?;
    let need = ["memory", "pids", "cpu"];
    if !need.iter().all(|c| ctl.split_whitespace().any(|x| x == *c)) { return None; }
    let sc_path = dir.join("cgroup.subtree_control");
    let sc = std::ffi::CString::new(sc_path.to_str()?).ok()?;
    if unsafe { libc::access(sc.as_ptr(), libc::W_OK) } != 0 { return None; }
    let enabled = |p: &Path| std::fs::read_to_string(p).map(|t| need.iter().all(|c| t.split_whitespace().any(|x| x == *c))).unwrap_or(false);
    if enabled(&sc_path) { return Some(dir); }
    // a leaf for ourselves, then the controllers for the siblings (best effort: any failure means "no delegation")
    let leaf = dir.join("zcode-main");
    std::fs::create_dir_all(&leaf).ok()?;
    std::fs::write(leaf.join("cgroup.procs"), std::process::id().to_string()).ok()?;
    for c in need { let _ = std::fs::write(&sc_path, format!("+{c}")); }
    enabled(&sc_path).then_some(dir)
}

pub fn check(cfg: &ContainerCfg) -> Result<Probe, String> {
    static CACHE: OnceLock<Mutex<Option<Result<Probe, String>>>> = OnceLock::new();
    let m = CACHE.get_or_init(Default::default);
    let mut g = m.lock().unwrap();
    if let Some(r) = g.as_ref() { return r.clone(); }
    let r = run(cfg);
    *g = Some(r.clone());
    r
}

fn run(cfg: &ContainerCfg) -> Result<Probe, String> {
    if cfg.guest.is_empty() {
        return Err("this zcode build has no embedded guest binary (build with the musl target installed, or set ZCODE_GUEST_BIN at build time)".into());
    }
    let runtime = which(&cfg.runtime).ok_or_else(|| format!("OCI runtime `{}` not found in PATH; install crun (or set container.runtime to runc/youki)", cfg.runtime))?;
    if std::fs::read_to_string("/proc/sys/user/max_user_namespaces").ok().and_then(|s| s.trim().parse::<u64>().ok()) == Some(0) {
        return Err("user namespaces are disabled (user.max_user_namespaces=0); enable them with sysctl".into());
    }
    if std::fs::read_to_string("/proc/sys/kernel/apparmor_restrict_unprivileged_userns").map(|s| s.trim() == "1").unwrap_or(false) {
        return Err("unprivileged user namespaces are restricted by AppArmor; run `sysctl kernel.apparmor_restrict_unprivileged_userns=0` or add an AppArmor profile for zcode".into());
    }
    if std::fs::read_to_string("/proc/sys/kernel/unprivileged_userns_clone").map(|s| s.trim() == "0").unwrap_or(false) {
        return Err("unprivileged user namespaces are disabled; run `sysctl kernel.unprivileged_userns_clone=1`".into());
    }
    // real test in a fresh single-threaded child: userns + mountns + overlayfs
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let out = std::process::Command::new(exe).arg("__ns").arg("--probe").output().map_err(|e| format!("cannot run the namespace probe: {e}"))?;
    let msg = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (mut overlay, mut userns) = (false, false);
    for l in msg.lines() {
        if l == "userns ok" { userns = true; }
        if l == "overlay ok" { overlay = true; }
    }
    if !userns { return Err(format!("cannot create a user+mount namespace ({}); unprivileged user namespaces must be allowed", String::from_utf8_lossy(&out.stderr).trim())); }
    let fuse = which("fuse-overlayfs");
    if !overlay && fuse.is_none() && cfg.rootfs != "copy" {
        return Err("overlayfs cannot be mounted inside a user namespace (needs Linux 5.11+) and `fuse-overlayfs` is not installed; install it or set container.rootfs to \"copy\" (slow start)".into());
    }
    Ok(Probe { runtime, overlay, fuse_overlayfs: fuse, cgroup: delegated_cgroup() })
}
