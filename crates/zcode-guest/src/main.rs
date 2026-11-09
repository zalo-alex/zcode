//! zcode-guest: the init of a tier 2 container. Pid 1 stays a tiny reaper that forwards signals; a
//! forked worker connects OUT to the host's per-agent unix socket and serves tool requests
//! (bash, background jobs, file tools) using the same implementation as tier 0. When the socket
//! closes the worker exits and so does the container.
use serde_json::{Value, json};
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use tokio::net::UnixStream;
use tokio::sync::mpsc;
use zcode_tools::frames::{chunks, read_frame, write_frame};
use zcode_tools::{ExecReq, LocalSandbox, Policy, Sandbox};

static WORKER: AtomicI32 = AtomicI32::new(0);

extern "C" fn forward(sig: libc::c_int) {
    let p = WORKER.load(Ordering::Relaxed);
    if p > 0 { unsafe { libc::kill(p, sig); } }
}

fn arg(name: &str) -> Option<String> {
    let a: Vec<String> = std::env::args().collect();
    a.iter().position(|x| x == name).and_then(|i| a.get(i + 1).cloned())
}

fn main() {
    let sock = arg("--connect").unwrap_or_else(|| "/run/zcode/g.sock".into());
    let cwd = arg("--cwd").unwrap_or_else(|| "/".into());
    unsafe {
        let pid = libc::fork();
        if pid < 0 { std::process::exit(70); }
        if pid == 0 { worker(&sock, &cwd); std::process::exit(0); }
        WORKER.store(pid, Ordering::Relaxed);
        for s in [libc::SIGTERM, libc::SIGINT, libc::SIGHUP] { libc::signal(s, forward as *const () as usize); }
        // reap every child (orphans re-parent to us); leave when the worker is gone
        loop {
            let mut st = 0;
            let r = libc::waitpid(-1, &mut st, 0);
            if r == pid {
                let code = if libc::WIFEXITED(st) { libc::WEXITSTATUS(st) } else { 128 + libc::WTERMSIG(st) };
                std::process::exit(code);
            }
            if r < 0 && *libc::__errno_location() != libc::EINTR { std::process::exit(0); }
        }
    }
}

/// Bring up loopback (the netns starts empty).
fn lo_up() {
    #[repr(C)]
    struct IfReq { name: [u8; 16], flags: i16, pad: [u8; 22] }
    unsafe {
        let s = libc::socket(libc::AF_INET, libc::SOCK_DGRAM | libc::SOCK_CLOEXEC, 0);
        if s < 0 { return; }
        let mut r = IfReq { name: [0; 16], flags: 0, pad: [0; 22] };
        r.name[..2].copy_from_slice(b"lo");
        if libc::ioctl(s, 0x8913, &mut r as *mut IfReq) == 0 {
            r.flags |= 0x1 | 0x40;
            libc::ioctl(s, 0x8914, &mut r as *mut IfReq);
        }
        libc::close(s);
    }
}

fn worker(sock: &str, cwd: &str) {
    lo_up();
    // network administration is only needed above; nothing we run afterwards may keep it
    unsafe { libc::prctl(libc::PR_CAPBSET_DROP, 12, 0, 0, 0); }
    let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().expect("runtime");
    rt.block_on(async move {
        let sb: Arc<dyn Sandbox> = Arc::new(LocalSandbox::open_none(Policy::new(cwd.into())));
        let mut tries = 0;
        let stream = loop {
            match UnixStream::connect(sock).await {
                Ok(s) => break s,
                Err(_) if tries < 100 => { tries += 1; tokio::time::sleep(std::time::Duration::from_millis(50)).await; }
                Err(_) => std::process::exit(71),
            }
        };
        let (mut rd, mut wr) = stream.into_split();
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        tokio::spawn(async move {
            while let Some(v) = rx.recv().await {
                if write_frame(&mut wr, &v).await.is_err() { std::process::exit(0); }
            }
        });
        loop {
            let Ok(req) = read_frame(&mut rd).await else { std::process::exit(0) };
            let (sb, tx) = (sb.clone(), tx.clone());
            tokio::spawn(async move { handle(sb, tx, req).await });
        }
    });
}

fn out(tx: &mpsc::UnboundedSender<Value>, id: u64, s: &str) {
    for c in chunks(s) { let _ = tx.send(json!({"id": id, "ev": "out", "data": c})); }
}

fn done(tx: &mpsc::UnboundedSender<Value>, id: u64, ok: bool, extra: Value) {
    let mut v = json!({"id": id, "ev": "done", "ok": ok});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) { for (k, x) in e { o.insert(k.clone(), x.clone()); } }
    let _ = tx.send(v);
}

async fn handle(sb: Arc<dyn Sandbox>, tx: mpsc::UnboundedSender<Value>, req: Value) {
    let id = req["id"].as_u64().unwrap_or(0);
    let op = req["op"].as_str().unwrap_or("");
    let a = &req["args"];
    let exec = |r: ExecReq| {
        let (sb, tx) = (sb.clone(), tx.clone());
        async move {
            match sb.exec(r).await {
                Ok(st) => {
                    let (text, d) = st.collect().await;
                    out(&tx, id, &text);
                    done(&tx, id, true, json!({"code": d.code, "timed_out": d.timed_out, "cwd": sb.cwd()}));
                }
                Err(e) => done(&tx, id, false, json!({"err": e.to_string()})),
            }
        }
    };
    let fs = sb.fs();
    let res = match op {
        "ping" => { out(&tx, id, "pong"); done(&tx, id, true, json!({})); return; }
        "shutdown" => {
            done(&tx, id, true, json!({}));
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            std::process::exit(0);
        }
        "exec" => {
            let secs = a["timeout"].as_u64().unwrap_or(120);
            exec(ExecReq::Run { command: a["command"].as_str().unwrap_or("").into(), timeout: std::time::Duration::from_secs(secs), background: a["background"].as_bool().unwrap_or(false) }).await;
            return;
        }
        "bash_output" => { exec(ExecReq::JobOutput { id: a["id"].as_str().map(String::from), lines: a["lines"].as_u64().unwrap_or(100) as usize }).await; return; }
        "bash_kill" => { exec(ExecReq::JobKill { id: a["id"].as_str().unwrap_or("").into() }).await; return; }
        "read" => fs.read(a).await,
        "write" => fs.write(a).await,
        "edit" => fs.edit(a).await,
        "multiedit" => fs.multiedit(a).await,
        "glob" => fs.glob(a).await,
        "grep" => fs.grep(a).await,
        "ls" => fs.ls(a).await,
        _ => { done(&tx, id, false, json!({"err": format!("unknown op `{op}`")})); return; }
    };
    match res {
        Ok(s) => { out(&tx, id, &s); done(&tx, id, true, json!({})); }
        Err(e) => done(&tx, id, false, json!({"err": e.to_string()})),
    }
}
