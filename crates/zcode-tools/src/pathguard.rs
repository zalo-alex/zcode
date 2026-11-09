//! Userland path policy for the file tools (tier 1). They run inside the zcode process, so Landlock
//! cannot confine them; every open goes through `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)`
//! against an allowed root, then the resulting fd is re-checked against the deny lists.
use crate::sandbox::{Policy, home};
use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};

const RESOLVE_NO_MAGICLINKS: u64 = 0x02;
const RESOLVE_BENEATH: u64 = 0x08;

struct Root {
    path: PathBuf,
    write: bool,
    fd: OwnedFd,
    depth: usize,
}

pub struct PathGuard {
    roots: Vec<Root>,
    /// never accessible, even under an explicit root
    hard_deny: Vec<PathBuf>,
    /// denied unless a root nested deeper than the entry covers the path (the rest of $HOME)
    soft_deny: Vec<PathBuf>,
}

pub fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => { out.pop(); }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn depth(p: &Path) -> usize { p.components().count() }

/// Normalize, then resolve symlinks of the longest existing prefix (the tail may not exist yet).
/// Only used to pick the root; the kernel enforces confinement with RESOLVE_BENEATH afterwards.
pub fn canon_prefix(p: &Path) -> PathBuf {
    let x = normalize(p);
    let mut cur = x.clone();
    let mut tail: Vec<std::ffi::OsString> = vec![];
    loop {
        if let Ok(c) = cur.canonicalize() { return tail.iter().rev().fold(c, |a, t| a.join(t)); }
        match (cur.file_name().map(|s| s.to_owned()), cur.parent().map(|p| p.to_path_buf())) {
            (Some(n), Some(par)) => { tail.push(n); cur = par; }
            _ => return x,
        }
    }
}

fn open_dirfd(p: &Path) -> io::Result<OwnedFd> {
    let c = cstr(p)?;
    let fd = unsafe { libc::open(c.as_ptr(), libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd) }) }
}

fn cstr(p: &Path) -> io::Result<std::ffi::CString> {
    std::ffi::CString::new(p.as_os_str().as_encoded_bytes()).map_err(|_| io::Error::from_raw_os_error(libc::EINVAL))
}

fn openat2(dir: i32, rel: &Path, flags: i32, mode: u32) -> io::Result<OwnedFd> {
    #[repr(C)] struct How { flags: u64, mode: u64, resolve: u64 }
    let how = How { flags: flags as u64 | libc::O_CLOEXEC as u64, mode: mode as u64, resolve: RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS };
    let rel = if rel.as_os_str().is_empty() { Path::new(".") } else { rel };
    let c = cstr(rel)?;
    let fd = unsafe { libc::syscall(libc::SYS_openat2, dir, c.as_ptr(), &how as *const How, std::mem::size_of::<How>()) };
    if fd < 0 { Err(io::Error::last_os_error()) } else { Ok(unsafe { OwnedFd::from_raw_fd(fd as i32) }) }
}

fn denied_err(p: &Path) -> io::Error {
    io::Error::new(io::ErrorKind::PermissionDenied, format!("{}: blocked by sandbox policy", p.display()))
}

impl PathGuard {
    pub fn new(p: &Policy, tmp: &Path) -> PathGuard {
        let canon = |x: &Path| x.canonicalize().unwrap_or_else(|_| normalize(x));
        let mut roots: Vec<Root> = vec![];
        let mut add = |path: PathBuf, write: bool| {
            let path = canon(&path);
            if let Ok(fd) = open_dirfd(&path) {
                let d = depth(&path);
                roots.push(Root { path, write, fd, depth: d });
            }
        };
        for d in ["/usr", "/bin", "/sbin", "/lib", "/lib32", "/lib64", "/libx32", "/etc"] { add(d.into(), false); }
        for d in &p.toolchain { add(d.clone(), false); }
        for d in &p.read { add(d.clone(), false); }
        add(p.cwd.clone(), false);
        for d in &p.write { add(d.clone(), true); }
        add(tmp.to_path_buf(), true);
        if p.outside { add("/".into(), true); }
        let hard: Vec<PathBuf> = Policy::hard_deny().iter().map(|d| canon(d)).chain(["/proc".into(), "/sys".into()]).collect();
        let soft = vec![canon(&home())];
        PathGuard { roots, hard_deny: hard, soft_deny: soft }
    }

    fn pick(&self, abs: &Path, write: bool) -> Option<&Root> {
        self.roots.iter().filter(|r| (!write || r.write) && abs.starts_with(&r.path)).max_by_key(|r| r.depth)
    }

    fn denied(&self, abs: &Path, root_depth: usize) -> bool {
        self.hard_deny.iter().any(|d| abs.starts_with(d))
            || self.soft_deny.iter().any(|d| abs.starts_with(d) && depth(d) > root_depth)
    }

    fn fd_path(fd: &OwnedFd) -> io::Result<PathBuf> { std::fs::read_link(format!("/proc/self/fd/{}", fd.as_raw_fd())) }

    /// Check `abs` (absolute) against the policy; returns (root fd, relative path, root depth).
    fn locate(&self, abs: &Path, write: bool) -> io::Result<(&Root, PathBuf)> {
        let abs = canon_prefix(abs);
        let root = self.pick(&abs, write).ok_or_else(|| denied_err(&abs))?;
        if self.denied(&abs, root.depth) { return Err(denied_err(&abs)); }
        let rel = abs.strip_prefix(&root.path).unwrap_or(Path::new("")).to_path_buf();
        Ok((root, rel))
    }

    fn verify(&self, fd: OwnedFd, shown: &Path, root: &Root) -> io::Result<OwnedFd> {
        let real = Self::fd_path(&fd)?;
        if self.denied(&real, root.depth) { return Err(denied_err(shown)); }
        Ok(fd)
    }

    pub fn open_read(&self, abs: &Path) -> io::Result<File> {
        let (root, rel) = self.locate(abs, false)?;
        let fd = openat2(root.fd.as_raw_fd(), &rel, libc::O_RDONLY, 0).map_err(|e| with_path(e, abs))?;
        Ok(File::from(self.verify(fd, abs, root)?))
    }

    /// Open for writing: creates (and truncates) the file.
    pub fn open_write(&self, abs: &Path) -> io::Result<File> {
        let (root, rel) = self.locate(abs, true)?;
        let fd = openat2(root.fd.as_raw_fd(), &rel, libc::O_WRONLY | libc::O_CREAT | libc::O_TRUNC, 0o666).map_err(|e| with_path(e, abs))?;
        Ok(File::from(self.verify(fd, abs, root)?))
    }

    /// Validate and open a directory for listing/walking.
    pub fn open_dir(&self, abs: &Path) -> io::Result<OwnedFd> {
        let (root, rel) = self.locate(abs, false)?;
        let fd = openat2(root.fd.as_raw_fd(), &rel, libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(|e| with_path(e, abs))?;
        self.verify(fd, abs, root)
    }

    pub fn list_dir(&self, abs: &Path) -> io::Result<Vec<(String, bool)>> {
        let fd = self.open_dir(abs)?;
        let mut v = vec![];
        for e in std::fs::read_dir(format!("/proc/self/fd/{}", fd.as_raw_fd()))?.flatten() {
            v.push((e.file_name().to_string_lossy().into_owned(), e.file_type().map(|t| t.is_dir()).unwrap_or(false)));
        }
        Ok(v)
    }

    /// `mkdir -p` confined to a writable root.
    pub fn mkdir_p(&self, abs: &Path) -> io::Result<()> {
        let (root, rel) = self.locate(abs, true)?;
        let mut cur = unsafe { OwnedFd::from_raw_fd(libc::dup(root.fd.as_raw_fd())) };
        for c in rel.components() {
            let Component::Normal(name) = c else { continue };
            let cn = cstr(Path::new(name))?;
            let r = unsafe { libc::mkdirat(cur.as_raw_fd(), cn.as_ptr(), 0o777) };
            if r != 0 {
                let e = io::Error::last_os_error();
                if e.raw_os_error() != Some(libc::EEXIST) { return Err(with_path(e, abs)); }
            }
            cur = openat2(cur.as_raw_fd(), Path::new(name), libc::O_RDONLY | libc::O_DIRECTORY, 0).map_err(|e| with_path(e, abs))?;
        }
        self.verify(cur, abs, root).map(|_| ())
    }

    /// Canonical path of an opened file (used as the stale-edit key).
    pub fn canon_of(&self, f: &File) -> Option<PathBuf> {
        std::fs::read_link(format!("/proc/self/fd/{}", f.as_raw_fd())).ok()
    }
}

fn with_path(e: io::Error, p: &Path) -> io::Error {
    // EXDEV from RESOLVE_BENEATH means an escape attempt
    if e.raw_os_error() == Some(libc::EXDEV) { return denied_err(p); }
    io::Error::new(e.kind(), format!("{}: {e}", p.display()))
}
