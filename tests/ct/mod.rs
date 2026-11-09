#![allow(dead_code)]
use crate::common::*;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

pub fn tool(name: &'static str, args: Value) -> Script { Script::Tools(vec![(name, args.to_string())]) }
pub fn text(s: &str) -> Script { Script::Text(s.into()) }
pub fn who(r: &Value) -> String {
    let sys = r["messages"][0]["content"].as_str().unwrap_or("");
    match sys.split_once("You are worker `") { Some((_, rest)) => rest.split('`').next().unwrap_or("?").to_string(), None => "lead".into() }
}
pub fn n_assist(r: &Value) -> usize { r["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").count() }
pub fn all_text(r: &Value) -> String {
    r["messages"].as_array().unwrap().iter().skip(1).map(|m| format!("{}|{}", m["role"], m["content"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")
}
pub fn tool_outputs(lines: &[Value], agent: &str, name: &str) -> Vec<String> {
    lines.iter().filter(|l| l["type"] == "tool_use" && l["agent"] == agent && l["part"]["tool"] == name)
        .map(|l| l["part"]["state"]["output"].as_str().unwrap_or("").to_string()).collect()
}

/// Static busybox rootfs, built once per test process.
pub fn fixture() -> Option<&'static Path> {
    static F: OnceLock<Option<PathBuf>> = OnceLock::new();
    F.get_or_init(|| {
        let bb = ["/usr/bin/busybox", "/bin/busybox"].iter().map(Path::new).find(|p| p.exists())?;
        let ldd = Command::new("ldd").arg(bb).output().ok()?;
        if !String::from_utf8_lossy(&ldd.stdout).contains("not a dynamic") && !String::from_utf8_lossy(&ldd.stderr).contains("not a dynamic") {
            eprintln!("SKIP: {} is not static", bb.display());
            return None;
        }
        let root = std::env::temp_dir().join(format!("zc_fixture_rootfs_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for d in ["bin", "etc", "root", "usr/bin"] { std::fs::create_dir_all(root.join(d)).ok()?; }
        std::fs::copy(bb, root.join("bin/busybox")).ok()?;
        Command::new(root.join("bin/busybox")).args(["--install", "-s", "/bin"]).env("PATH", "/bin").current_dir(&root).output().ok();
        // `--install -s /bin` would write on the host if not careful: do it relative to the fixture instead
        let st = Command::new(root.join("bin/busybox")).arg("--list").output().ok()?;
        for app in String::from_utf8_lossy(&st.stdout).lines() {
            let p = root.join("bin").join(app);
            if !p.exists() { let _ = std::os::unix::fs::symlink("busybox", &p); }
        }
        std::fs::write(root.join("etc/passwd"), "root:x:0:0:root:/root:/bin/sh\n").ok()?;
        Some(root)
    }).as_deref()
}

/// None = skip (reason printed).
pub fn setup(name: &str, cfg: Value) -> Option<(Env, Value)> {
    if Command::new("crun").arg("--version").output().is_err() { eprintln!("SKIP {name}: crun not installed"); return None; }
    let Some(fx) = fixture() else { eprintln!("SKIP {name}: no static busybox for the fixture rootfs"); return None };
    let probe = Command::new(env!("CARGO_BIN_EXE_zcode")).args(["__ns", "--probe"]).output().ok()?;
    let p = String::from_utf8_lossy(&probe.stdout);
    if !p.contains("userns ok") { eprintln!("SKIP {name}: cannot create user namespaces: {}", String::from_utf8_lossy(&probe.stderr).trim()); return None; }
    if !p.contains("overlay ok") && Command::new("fuse-overlayfs").arg("--version").output().is_err() {
        eprintln!("SKIP {name}: no overlayfs in userns and no fuse-overlayfs"); return None;
    }
    let e = Env::new(name);
    let imp = Command::new(env!("CARGO_BIN_EXE_zcode")).args(["image", "import"]).arg(fx).args(["--name", "bb"])
        .env("HOME", &e.home).env("XDG_CACHE_HOME", e.home.join("cache")).env("XDG_STATE_HOME", e.home.join("state")).output().unwrap();
    assert!(imp.status.success(), "image import: {}", String::from_utf8_lossy(&imp.stderr));
    let mut c = json!({"container": {"image": "bb", "runtime": "crun"}});
    if let (Some(a), Some(b)) = (c.as_object_mut(), cfg.as_object()) { for (k, v) in b { if k == "container" { for (k2, v2) in v.as_object().unwrap() { a["container"][k2] = v2.clone(); } } else { a.insert(k.clone(), v.clone()); } } }
    Some((e, c))
}

pub fn cmd(e: &Env, llm: &FakeLlm, cfg: Value, extra: &[&str]) -> Command {
    let mut args: Vec<&str> = vec!["run", "--auto", "--format", "json"];
    args.extend_from_slice(extra);
    args.push("go");
    let mut c = e.cmd_cfg(llm, cfg, &args);
    c.env("XDG_STATE_HOME", e.home.join("state")).env("XDG_RUNTIME_DIR", e.dir.join("runtime"));
    std::fs::create_dir_all(e.dir.join("runtime")).ok();
    c
}

pub fn skip_if_unavailable(o: &std::process::Output) -> bool {
    let s = err(o) + &out(o);
    if s.contains("tier 2 sandbox cannot be enforced") || s.contains("tier 2 guest") || s.contains("no embedded guest") {
        eprintln!("SKIP: tier 2 unavailable: {}", s.lines().find(|l| l.contains("cannot be enforced") || l.contains("guest")).unwrap_or(""));
        return true;
    }
    false
}

