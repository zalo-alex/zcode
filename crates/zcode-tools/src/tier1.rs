//! Tier 1 confinement of the bash child: Landlock (raw syscalls, so the `pre_exec` closure never
//! allocates) + a seccomp denylist. Used ONLY by tier 1. Everything that can allocate or open files
//! happens in the parent (`Prepared::build`); the child only issues raw syscalls.
use crate::sandbox::{Policy, home};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

// Landlock constants (include/uapi/linux/landlock.h)
const SYS_LL_CREATE: libc::c_long = 444;
const SYS_LL_ADD: libc::c_long = 445;
const SYS_LL_RESTRICT: libc::c_long = 446;
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
const NET_ALL: u64 = 0b11; // BIND_TCP | CONNECT_TCP
const SCOPE_ALL: u64 = 0b11; // abstract unix socket | signal
/// rights that make sense on a non-directory
const FILE_ONLY: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

fn handled_fs(abi: u32) -> u64 {
    let mut m = (1u64 << 13) - 1; // bits 0..=12
    if abi >= 2 { m |= FS_REFER; }
    if abi >= 3 { m |= FS_TRUNCATE; }
    if abi >= 5 { m |= FS_IOCTL_DEV; }
    m
}

#[repr(C)]
struct RulesetAttr { handled_fs: u64, handled_net: u64, scoped: u64 }
#[repr(C, packed)]
struct PathBeneathAttr { allowed: u64, parent_fd: i32 }

#[derive(Clone, Debug)]
pub struct Caps {
    pub abi: u32,
}

/// Detect kernel support once, lazily (never at `--version`/banner time).
pub fn probe() -> Result<Caps, String> {
    static C: OnceLock<Result<Caps, String>> = OnceLock::new();
    C.get_or_init(|| {
        let abi = unsafe { libc::syscall(SYS_LL_CREATE, std::ptr::null::<u8>(), 0usize, 1u32) };
        if abi < 1 {
            let e = std::io::Error::last_os_error();
            return Err(format!("Landlock is not available on this kernel ({e}; check `cat /sys/kernel/security/lsm` includes landlock)"));
        }
        // seccomp filter mode
        let r = unsafe { libc::prctl(libc::PR_GET_SECCOMP) };
        if r < 0 { return Err("seccomp is not available on this kernel".into()); }
        // openat2 (5.6+) for the path-checked file tools
        #[repr(C)] struct How { flags: u64, mode: u64, resolve: u64 }
        let how = How { flags: libc::O_RDONLY as u64 | libc::O_DIRECTORY as u64, mode: 0, resolve: 0x08 };
        let fd = unsafe { libc::syscall(libc::SYS_openat2, libc::AT_FDCWD, c".".as_ptr(), &how as *const How, std::mem::size_of::<How>()) };
        if fd < 0 { return Err(format!("openat2 is not available ({})", std::io::Error::last_os_error())); }
        unsafe { libc::close(fd as i32); }
        if !Path::new("/proc/self/fd").exists() { return Err("/proc is not mounted".into()); }
        Ok(Caps { abi: abi as u32 })
    }).clone()
}

struct Rule { fd: OwnedFd, access: u64 }

/// Everything the child needs, built in the parent.
pub struct Prepared {
    handled_fs: u64,
    handled_net: u64,
    scoped: u64,
    attr_size: usize,
    rules: Vec<Rule>,
    proc_whole: Option<OwnedFd>,
    proc_ro: u64,
    bpf: seccompiler::BpfProgram,
    nofile: Option<u64>,
    core: Option<u64>,
}

fn open_path(p: &Path) -> Option<OwnedFd> {
    let c = std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).ok()?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_CLOEXEC) };
    if fd < 0 { None } else { Some(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

fn is_dir(fd: &OwnedFd) -> bool {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    unsafe { libc::fstat(fd.as_raw_fd(), &mut st) == 0 && (st.st_mode & libc::S_IFMT) == libc::S_IFDIR }
}

pub fn canon(p: &Path) -> PathBuf { p.canonicalize().unwrap_or_else(|_| p.to_path_buf()) }

/// Refuse a policy that grants a root which contains an always-denied directory (Landlock cannot carve holes).
pub fn check_deny_conflicts(roots: &[PathBuf]) -> Result<(), String> {
    for d in Policy::hard_deny() {
        let dc = canon(&d);
        for r in roots {
            let rc = canon(r);
            if dc.starts_with(&rc) && dc.exists() {
                return Err(format!("{} contains {} which must stay inaccessible; run from a project subdirectory or use `--sandbox none`", rc.display(), dc.display()));
            }
            if rc.starts_with(&dc) {
                return Err(format!("{} is inside {} which is always denied", rc.display(), dc.display()));
            }
        }
    }
    Ok(())
}

impl Prepared {
    /// The read-write and read-only roots of a policy.
    fn roots(p: &Policy, tmp: &Path) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let mut roots_rw: Vec<PathBuf> = p.write.clone();
        roots_rw.push(tmp.to_path_buf());
        let mut roots_ro: Vec<PathBuf> = p.read.clone();
        roots_ro.push(p.cwd.clone());
        for d in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc"] { roots_ro.push(d.into()); }
        roots_ro.extend(p.toolchain.iter().cloned());
        (roots_rw, roots_ro)
    }

    /// The cheap part of `build` (no fds, no filter): refuse a policy that cannot be enforced. Run at open time so the error is immediate.
    pub fn validate(p: &Policy, tmp: &Path) -> Result<(), String> {
        let (roots_rw, roots_ro) = Self::roots(p, tmp);
        let mut all = roots_rw;
        all.extend(roots_ro.iter().filter(|r| r.starts_with(&p.cwd) || p.read.contains(r)).cloned());
        check_deny_conflicts(&all)
    }

    pub fn build(p: &Policy, tmp: &Path, caps: &Caps) -> Result<Prepared, String> {
        let abi = caps.abi.min(6);
        let hf = handled_fs(abi);
        let ro = (FS_EXECUTE | FS_READ_FILE | FS_READ_DIR) & hf;
        let rw = hf;
        Self::validate(p, tmp)?;
        let (roots_rw, roots_ro) = Self::roots(p, tmp);
        let mut rules = vec![];
        for (path, access) in roots_ro.iter().map(|r| (r, ro)).chain(roots_rw.iter().map(|r| (r, rw))) {
            let Some(fd) = open_path(path) else { continue };
            let access = if is_dir(&fd) { access } else { access & FILE_ONLY };
            rules.push(Rule { fd, access });
        }
        // devices
        for d in ["/dev/null", "/dev/zero", "/dev/urandom", "/dev/random", "/dev/tty"] {
            if let Some(fd) = open_path(Path::new(d)) {
                rules.push(Rule { fd, access: (FS_READ_FILE | FS_WRITE_FILE | FS_IOCTL_DEV) & hf });
            }
        }
        // minimal /proc: the child's own /proc/<pid> is added in the child; plus a few harmless globals
        for f in ["/proc/cpuinfo", "/proc/meminfo", "/proc/stat", "/proc/loadavg", "/proc/uptime", "/proc/version", "/proc/filesystems", "/proc/sys"] {
            if let Some(fd) = open_path(Path::new(f)) {
                let a = if is_dir(&fd) { ro } else { ro & FILE_ONLY };
                rules.push(Rule { fd, access: a });
            }
        }
        let proc_whole = if p.proc { open_path(Path::new("/proc")) } else { None };
        let handled_net = if !p.net && abi >= 4 { NET_ALL } else { 0 };
        let scoped = if abi >= 6 { SCOPE_ALL } else { 0 };
        let attr_size = if abi >= 6 { 24 } else if abi >= 4 { 16 } else { 8 };
        Ok(Prepared { handled_fs: hf, handled_net, scoped, attr_size, rules, proc_whole, proc_ro: ro, bpf: build_seccomp(p.net)?,
            nofile: p.limits.nofile, core: p.limits.core })
    }


    /// Runs in the forked child before exec. Only raw syscalls on pre-built data; no allocation.
    pub fn confine_child(&self) -> std::io::Result<()> {
        unsafe {
            if let Some(n) = self.nofile { let r = libc::rlimit { rlim_cur: n as _, rlim_max: n as _ }; libc::setrlimit(libc::RLIMIT_NOFILE, &r); }
            if let Some(n) = self.core { let r = libc::rlimit { rlim_cur: n as _, rlim_max: n as _ }; libc::setrlimit(libc::RLIMIT_CORE, &r); }
            if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 { return Err(std::io::Error::last_os_error()); }
            let attr = RulesetAttr { handled_fs: self.handled_fs, handled_net: self.handled_net, scoped: self.scoped };
            let rfd = libc::syscall(SYS_LL_CREATE, &attr as *const RulesetAttr, self.attr_size, 0u32) as i32;
            if rfd < 0 { return Err(std::io::Error::last_os_error()); }
            for r in &self.rules {
                let pb = PathBeneathAttr { allowed: r.access, parent_fd: r.fd.as_raw_fd() };
                if libc::syscall(SYS_LL_ADD, rfd, 1u32, &pb as *const PathBeneathAttr, 0u32) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            // /proc/<own pid>, formatted into a stack buffer
            let mut buf = [0u8; 32];
            buf[..6].copy_from_slice(b"/proc/");
            let mut n = libc::getpid() as u32;
            let mut digits = [0u8; 12];
            let mut i = 0;
            while n > 0 { digits[i] = b'0' + (n % 10) as u8; n /= 10; i += 1; }
            let mut o = 6;
            while i > 0 { i -= 1; buf[o] = digits[i]; o += 1; }
            let pfd = libc::open(buf.as_ptr() as *const libc::c_char, libc::O_PATH | libc::O_CLOEXEC);
            if pfd >= 0 {
                let pb = PathBeneathAttr { allowed: self.proc_ro, parent_fd: pfd };
                libc::syscall(SYS_LL_ADD, rfd, 1u32, &pb as *const PathBeneathAttr, 0u32);
                libc::close(pfd);
            }
            if let Some(w) = &self.proc_whole {
                let pb = PathBeneathAttr { allowed: self.proc_ro, parent_fd: w.as_raw_fd() };
                libc::syscall(SYS_LL_ADD, rfd, 1u32, &pb as *const PathBeneathAttr, 0u32);
            }
            if libc::syscall(SYS_LL_RESTRICT, rfd, 0u32) != 0 { return Err(std::io::Error::last_os_error()); }
            libc::close(rfd);
        }
        seccompiler::apply_filter(&self.bpf).map_err(|_| std::io::Error::other("seccomp"))?;
        Ok(())
    }
}

fn build_seccomp(net: bool) -> Result<seccompiler::BpfProgram, String> {
    use seccompiler::*;
    use std::collections::BTreeMap;
    let deny: &[libc::c_long] = &[
        libc::SYS_ptrace, libc::SYS_process_vm_readv, libc::SYS_process_vm_writev, libc::SYS_mount, libc::SYS_umount2,
        libc::SYS_pivot_root, libc::SYS_unshare, libc::SYS_setns, libc::SYS_bpf, libc::SYS_keyctl, libc::SYS_add_key,
        libc::SYS_request_key, libc::SYS_kexec_load, libc::SYS_perf_event_open, libc::SYS_io_uring_setup,
        libc::SYS_io_uring_enter, libc::SYS_io_uring_register, libc::SYS_userfaultfd,
    ];
    let mut rules: BTreeMap<i64, Vec<SeccompRule>> = BTreeMap::new();
    for s in deny { rules.insert(*s, vec![]); }
    if !net {
        let mut v = vec![];
        for af in [libc::AF_INET, libc::AF_INET6] {
            let c = SeccompCondition::new(0, SeccompCmpArgLen::Dword, SeccompCmpOp::Eq, af as u64).map_err(|e| e.to_string())?;
            v.push(SeccompRule::new(vec![c]).map_err(|e| e.to_string())?);
        }
        rules.insert(libc::SYS_socket, v);
    }
    let arch: TargetArch = std::env::consts::ARCH.try_into().map_err(|e: BackendError| e.to_string())?;
    let f = SeccompFilter::new(rules, SeccompAction::Allow, SeccompAction::Errno(libc::EPERM as u32), arch).map_err(|e| e.to_string())?;
    f.try_into().map_err(|e: BackendError| e.to_string())
}

/// Environment passed to the tier 1 bash child.
pub fn child_env(p: &Policy, tmp: &Path) -> Vec<(String, String)> {
    let mut v: Vec<(String, String)> = vec![];
    let mut keep = |k: &str| if let Ok(x) = std::env::var(k) { v.push((k.to_string(), x)); };
    for k in ["PATH", "LANG", "LC_ALL", "TERM", "USER", "CARGO_HOME", "RUSTUP_HOME"] { keep(k); }
    for k in &p.env {
        let u = k.to_uppercase();
        if u.starts_with("ZCODE_") || u.starts_with("OPENCODE_") || ["KEY", "TOKEN", "SECRET", "PASSWORD"].iter().any(|w| u.contains(w)) { continue; }
        keep(k);
    }
    v.push(("HOME".into(), home().display().to_string()));
    v.push(("TMPDIR".into(), tmp.display().to_string()));
    v
}
