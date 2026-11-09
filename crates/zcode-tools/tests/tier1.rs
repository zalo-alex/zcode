//! Tier 1 enforcement tests. They need Landlock + seccomp + openat2; on a kernel without them each test
//! prints a visible SKIP reason instead of failing.
#![cfg(target_os = "linux")]
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zcode_tools::*;

struct T {
    sb: LocalSandbox,
    work: PathBuf,
    other: PathBuf,
}

fn dirs(name: &str) -> (PathBuf, PathBuf) {
    let base = std::env::temp_dir().join(format!("zt_t1_{name}_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let (work, other) = (base.join("work"), base.join("other"));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("secret"), "TOPSECRET").unwrap();
    (work.canonicalize().unwrap(), other.canonicalize().unwrap())
}

fn setup(name: &str, net: bool) -> Option<T> {
    let (work, other) = dirs(name);
    let mut p = Policy::new(work.clone());
    p.net = net;
    match LocalSandbox::open_tier1(p) {
        Ok(sb) => Some(T { sb, work, other }),
        Err(e) => { eprintln!("SKIP {name}: tier 1 unavailable: {e}"); None }
    }
}

async fn sh(t: &T, cmd: &str) -> (String, i32) {
    let (out, done) = t.sb.exec(ExecReq::Run { command: cmd.into(), timeout: Duration::from_secs(60), background: false })
        .await.unwrap().collect().await;
    (out, done.code.unwrap_or(-1))
}

fn p(x: &Path) -> String { x.display().to_string() }

#[tokio::test]
async fn cannot_read_outside_or_write_outside() {
    let Some(t) = setup("rw", false) else { return };
    let (o, c) = sh(&t, &format!("cat {}/secret", p(&t.other))).await;
    assert!(c != 0 && !o.contains("TOPSECRET") && o.contains("Permission denied"), "{o}");
    let (o, c) = sh(&t, &format!("echo x > {}/new", p(&t.other))).await;
    assert!(c != 0 && !t.other.join("new").exists(), "{o}");
    // allowed: cwd, private tmp, /dev/null, reads of /usr and /etc
    let (o, c) = sh(&t, "echo hi > a.txt && cat a.txt && echo z > $TMPDIR/t && echo n > /dev/null && ls /usr/bin | head -1 && cat /etc/passwd | head -1").await;
    assert_eq!(c, 0, "{o}");
    assert!(o.starts_with("hi"), "{o}");
    // $HOME itself and its config are not reachable
    let (o, c) = sh(&t, "ls ~/.ssh ~/.config/zcode 2>&1; cat ~/.ssh/id_rsa 2>&1").await;
    assert!(c != 0 || o.contains("denied") || o.contains("No such"), "{o}");
    // the zcode process itself is not restricted
    assert_eq!(std::fs::read_to_string(t.other.join("secret")).unwrap(), "TOPSECRET");
    t.sb.close().await;
}

#[tokio::test]
async fn symlink_and_dotdot_escapes_are_blocked() {
    let Some(t) = setup("esc", false) else { return };
    std::os::unix::fs::symlink(&t.other, t.work.join("link")).unwrap();
    let (o, _) = sh(&t, "cat link/secret; cat ../other/secret; cat ./../other/secret").await;
    assert!(!o.contains("TOPSECRET") && o.contains("denied"), "{o}");
    // file tools
    let fs = t.sb.fs();
    for path in ["link/secret", "../other/secret", &format!("{}/../other/secret", p(&t.work)), &format!("{}/secret", p(&t.other))] {
        let e = fs.read(&json!({"path": path})).await.unwrap_err().to_string();
        assert!(e.contains("blocked") || e.contains("denied") || e.contains("No such"), "{path}: {e}");
    }
    assert!(fs.write(&json!({"path":"link/new","content":"x"})).await.is_err());
    assert!(!t.other.join("new").exists());
    assert!(fs.write(&json!({"path":"../other/new","content":"x"})).await.is_err());
    assert!(fs.ls(&json!({"path":"link"})).await.is_err());
    assert!(fs.grep(&json!({"pattern":"TOP","path":"link"})).await.is_err());
    // grep over the project must not follow the symlink out
    let g = fs.grep(&json!({"pattern":"TOPSECRET"})).await.unwrap();
    assert_eq!(g, "no matches");
    // inside works
    fs.write(&json!({"path":"sub/x.txt","content":"hello"})).await.unwrap();
    assert!(fs.read(&json!({"path":"sub/x.txt"})).await.unwrap().contains("hello"));
    // /proc is never readable through the file tools
    assert!(fs.read(&json!({"path":"/proc/self/environ"})).await.is_err());
    t.sb.close().await;
}

#[tokio::test]
async fn network_syscalls_and_ptrace_are_denied() {
    let Some(t) = setup("net", false) else { return };
    let (o, _) = sh(&t, "bash -c 'exec 3<>/dev/tcp/127.0.0.1/9' 2>&1").await;
    assert!(o.contains("not permitted") || o.contains("Permission denied"), "AF_INET must fail: {o}");
    // unix sockets and pipes still work
    let (o, c) = sh(&t, "echo ok | cat").await;
    assert!(c == 0 && o.trim() == "ok", "{o}");
    t.sb.close().await;
    let Some(t2) = setup("net2", true) else { return };
    let (o, _) = sh(&t2, "bash -c 'exec 3<>/dev/tcp/127.0.0.1/9' 2>&1").await;
    assert!(o.contains("refused"), "with net=true the connect reaches the stack: {o}");
    t2.sb.close().await;
}

#[tokio::test]
async fn ptrace_and_build_inside_sandbox() {
    let Some(t) = setup("cc", false) else { return };
    std::fs::write(t.work.join("pt.c"), "#include <sys/ptrace.h>\n#include <stdio.h>\n#include <errno.h>\nint main(){long r=ptrace(PTRACE_TRACEME,0,0,0);printf(\"r=%ld errno=%d\\n\",r,errno);return 0;}\n").unwrap();
    let (o, c) = sh(&t, "gcc -o pt pt.c && ./pt").await;
    if o.contains("not found") { eprintln!("SKIP: no gcc"); return; }
    assert_eq!(c, 0, "build must work in the sandbox: {o}");
    assert!(o.contains("r=-1") && o.contains("errno=1"), "ptrace must be EPERM: {o}");
    t.sb.close().await;
}

#[tokio::test]
async fn environment_is_scrubbed() {
    let Some(t) = setup("env", false) else { return };
    let (o, _) = sh(&t, "env").await;
    for l in o.lines() {
        let k = l.split('=').next().unwrap_or("");
        assert!(!k.starts_with("ZCODE_") && !k.starts_with("OPENCODE_") && !k.contains("KEY") && !k.contains("TOKEN"), "leaked {l}");
    }
    assert!(o.contains("TMPDIR="), "{o}");
    // the parent's environ is not readable (dumpable=0 + landlock)
    let (o, _) = sh(&t, "cat /proc/$PPID/environ 2>&1 | head -c 100").await;
    assert!(o.contains("denied") || o.is_empty() || o.contains("No such"), "{o}");
    t.sb.close().await;
}

#[tokio::test]
async fn stale_edit_detection() {
    let Some(t) = setup("stale", false) else { return };
    let fs = t.sb.fs();
    std::fs::write(t.work.join("f.txt"), "one\n").unwrap();
    fs.read(&json!({"path":"f.txt"})).await.unwrap();
    std::fs::write(t.work.join("f.txt"), "two\n").unwrap(); // someone else
    let e = fs.edit(&json!({"path":"f.txt","old_string":"two","new_string":"3"})).await.unwrap_err().to_string();
    assert!(e.contains(STALE), "{e}");
    let e = fs.write(&json!({"path":"f.txt","content":"x"})).await.unwrap_err().to_string();
    assert!(e.contains(STALE), "{e}");
    let e = fs.multiedit(&json!({"path":"f.txt","edits":[{"old_string":"two","new_string":"3"}]})).await.unwrap_err().to_string();
    assert!(e.contains(STALE), "{e}");
    fs.read(&json!({"path":"f.txt"})).await.unwrap();
    fs.edit(&json!({"path":"f.txt","old_string":"two","new_string":"3"})).await.unwrap();
    // our own edits do not trip the check
    fs.edit(&json!({"path":"f.txt","old_string":"3","new_string":"4"})).await.unwrap();
    t.sb.close().await;
}

#[tokio::test]
async fn open_fails_closed_when_required_tier_is_unavailable() {
    let (work, _) = dirs("fc");
    let r = open(Mode::None, Policy::new(work), false, true);
    assert!(r.is_err(), "--require-sandbox with --sandbox none must fail");
}

#[tokio::test]
async fn cwd_containing_denied_dir_is_refused() {
    let (work, _) = dirs("deny");
    let mut p = Policy::new(work);
    let home = std::env::var("HOME").unwrap();
    p.cwd = PathBuf::from(&home);
    p.write = vec![PathBuf::from(&home)];
    if !Path::new(&home).join(".ssh").exists() && !Path::new(&home).join(".config/zcode").exists() { eprintln!("SKIP: no denied dirs under HOME"); return; }
    assert!(LocalSandbox::open_tier1(p).is_err());
}
