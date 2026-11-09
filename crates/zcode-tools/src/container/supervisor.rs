//! `zcode __ns`: a short-lived, SINGLE-THREADED re-exec of the zcode binary (unshare(CLONE_NEWUSER) is
//! impossible in a multi-threaded process). It creates the user+mount namespace (host uid mapped to
//! container root), mounts the per-agent root filesystem, then drives the OCI runtime CLI:
//! `create`, `start`, and (when its stdin closes, i.e. zcode went away or closed the agent) `kill` + `delete`.
use serde_json::Value;
use std::ffi::CString;
use std::io::{Read, Write};
use std::path::Path;
use std::process::{Command, Stdio};

fn cs(s: &str) -> CString { CString::new(s).unwrap_or_default() }

fn enter_userns() -> Result<(), String> {
    let (uid, gid) = unsafe { (libc::getuid(), libc::getgid()) };
    if unsafe { libc::unshare(libc::CLONE_NEWUSER | libc::CLONE_NEWNS) } != 0 {
        return Err(format!("unshare(user+mount): {}", std::io::Error::last_os_error()));
    }
    let w = |p: &str, c: String| std::fs::write(p, c).map_err(|e| format!("{p}: {e}"));
    w("/proc/self/setgroups", "deny".into())?;
    w("/proc/self/uid_map", format!("0 {uid} 1"))?;
    w("/proc/self/gid_map", format!("0 {gid} 1"))?;
    let r = unsafe { libc::mount(std::ptr::null(), c"/".as_ptr(), std::ptr::null(), libc::MS_REC | libc::MS_PRIVATE, std::ptr::null()) };
    if r != 0 { return Err(format!("make mounts private: {}", std::io::Error::last_os_error())); }
    Ok(())
}

fn mount_overlay(lower: &str, upper: &str, work: &str, merged: &str) -> Result<(), String> {
    let data = cs(&format!("lowerdir={lower},upperdir={upper},workdir={work}"));
    let r = unsafe { libc::mount(c"overlay".as_ptr(), cs(merged).as_ptr(), c"overlay".as_ptr(), 0, data.as_ptr() as *const libc::c_void) };
    if r != 0 { return Err(format!("mount overlay: {}", std::io::Error::last_os_error())); }
    Ok(())
}

/// `zcode __ns --probe`: report what this kernel allows.
fn probe_main() -> i32 {
    if let Err(e) = enter_userns() { eprintln!("{e}"); return 1; }
    println!("userns ok");
    let base = std::env::temp_dir().join(format!("zc-probe-{}", std::process::id()));
    for d in ["l", "u", "w", "m"] { let _ = std::fs::create_dir_all(base.join(d)); }
    let p = |d: &str| base.join(d).display().to_string();
    if mount_overlay(&p("l"), &p("u"), &p("w"), &p("m")).is_ok() { println!("overlay ok"); }
    let _ = std::io::stdout().flush();
    super::force_remove(&base);
    0
}

fn runtime_cmd(c: &Value, root: &str) -> Command {
    let mut cmd = Command::new(c["runtime"].as_str().unwrap_or("crun"));
    if let Some(a) = c["runtime_args"].as_array() { for x in a.iter().filter_map(|x| x.as_str()) { cmd.arg(x); } }
    cmd.arg("--root").arg(root);
    cmd
}

fn fail(m: &str) -> i32 {
    println!("error: {m}");
    let _ = std::io::stdout().flush();
    1
}

fn run_main(spec: &str) -> i32 {
    let Ok(c) = serde_json::from_str::<Value>(spec) else { return fail("bad supervisor spec") };
    let s = |k: &str| c[k].as_str().unwrap_or("").to_string();
    if let Err(e) = enter_userns() { return fail(&e); }
    let plan = &c["plan"];
    let ps = |k: &str| plan[k].as_str().unwrap_or("").to_string();
    match plan["kind"].as_str().unwrap_or("") {
        "overlay" => if let Err(e) = mount_overlay(&ps("lower"), &ps("upper"), &ps("work"), &ps("merged")) { return fail(&e); },
        "fuse" => {
            let o = Command::new(c["fuse"].as_str().unwrap_or("fuse-overlayfs"))
                .arg("-o").arg(format!("lowerdir={},upperdir={},workdir={}", ps("lower"), ps("upper"), ps("work"))).arg(ps("merged"))
                .stdin(Stdio::null()).output();
            match o { Ok(o) if o.status.success() => {}, Ok(o) => return fail(&format!("fuse-overlayfs: {}", String::from_utf8_lossy(&o.stderr).trim())), Err(e) => return fail(&format!("fuse-overlayfs: {e}")) }
        }
        _ => {} // copy: the host already prepared a private rootfs copy
    }
    let root = s("root");
    let log = std::fs::File::create(s("log")).ok();
    let stdio = |l: &Option<std::fs::File>| l.as_ref().and_then(|f| f.try_clone().ok()).map(Stdio::from).unwrap_or_else(Stdio::null);
    let st = runtime_cmd(&c, &root).args(["create", "--bundle", &s("bundle"), &s("id")]).stdin(Stdio::null()).stdout(stdio(&log)).stderr(stdio(&log)).status();
    match st {
        Ok(x) if x.success() => {}
        Ok(x) => { let t = std::fs::read_to_string(s("log")).unwrap_or_default(); return fail(&format!("runtime create failed ({x}): {}", t.trim().lines().last().unwrap_or(""))); }
        Err(e) => return fail(&format!("cannot run the OCI runtime: {e}")),
    }
    let st = runtime_cmd(&c, &root).args(["start", &s("id")]).stdin(Stdio::null()).stdout(stdio(&log)).stderr(stdio(&log)).status();
    if !st.map(|x| x.success()).unwrap_or(false) {
        let t = std::fs::read_to_string(s("log")).unwrap_or_default();
        let _ = runtime_cmd(&c, &root).args(["delete", "-f", &s("id")]).status();
        return fail(&format!("runtime start failed: {}", t.trim().lines().last().unwrap_or("")));
    }
    println!("ready");
    let _ = std::io::stdout().flush();
    // the container lives while our stdin is open (zcode alive); EOF = clean up
    let mut buf = [0u8; 64];
    let mut stdin = std::io::stdin();
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    let _ = runtime_cmd(&c, &root).args(["kill", &s("id"), "KILL"]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    // give the init a moment to die before delete
    for _ in 0..20 {
        let out = runtime_cmd(&c, &root).args(["state", &s("id")]).stdin(Stdio::null()).stderr(Stdio::null()).output();
        if out.map(|o| !String::from_utf8_lossy(&o.stdout).contains("\"running\"")).unwrap_or(true) { break; }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    let _ = runtime_cmd(&c, &root).args(["delete", "-f", &s("id")]).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).status();
    if Path::new(&ps("merged")).exists() { /* the mount dies with this namespace */ }
    0
}

/// Entry point called first thing in `main`: returns Some(exit code) when this process is a supervisor.
pub fn maybe_run() -> Option<i32> {
    let a: Vec<String> = std::env::args().collect();
    if a.get(1).map(String::as_str) != Some("__ns") { return None; }
    Some(match a.get(2).map(String::as_str) {
        Some("--probe") => probe_main(),
        Some(spec) => run_main(spec),
        None => 2,
    })
}
