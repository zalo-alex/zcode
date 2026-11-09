//! Tier 2 (container) tests. They need crun, unprivileged user namespaces, overlayfs-in-userns (or fuse-overlayfs)
//! and a static busybox for the fixture rootfs; when something is missing each test prints a visible SKIP reason.
mod common;
mod ct;
use common::*;
use ct::*;
use serde_json::json;
use std::path::Path;

#[test]
fn lead_in_a_container_runs_tools_inside_and_files_stay_owned_by_host_user() {
    let Some((e, cfg)) = setup("c_basic", json!({})) else { return };
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("bash", json!({"command":"echo inside; id -u; pwd; echo x > made.txt; echo marker > /rootmark; ls /"})),
        1 => tool("write", json!({"path":"w.txt","content":"hello\n"})),
        2 => tool("read", json!({"path":"w.txt"})),
        3 => tool("edit", json!({"path":"w.txt","old_string":"hello","new_string":"bye"})),
        _ => text("fin"),
    });
    let o = cmd(&e, &llm, cfg, &["--sandbox", "container"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let l = json_lines(&o);
    let b = tool_outputs(&l, "lead", "bash").concat();
    assert!(b.contains("inside") && b.contains("0\n") && b.contains(&e.cwd.display().to_string()), "{b}");
    assert!(b.contains("rootmark"), "private root fs is writable: {b}");
    assert_eq!(std::fs::read_to_string(e.cwd.join("w.txt")).unwrap(), "bye\n");
    use std::os::unix::fs::MetadataExt;
    assert_eq!(std::fs::metadata(e.cwd.join("made.txt")).unwrap().uid(), unsafe { libc::getuid() });
    assert!(!Path::new("/rootmark").exists(), "root fs changes never reach the host");
}

#[test]
fn lead_tier1_with_container_worker_mixed_tiers() {
    let Some((e, cfg)) = setup("c_mixed", json!({"profiles":{"box":{"sandbox":"container"}}})) else { return };
    let llm = FakeLlm::start_fn(|r| {
        let (w, all, n) = (who(r), all_text(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", _) if all.contains("[from w] worker done") => text("fin"),
            ("lead", 0) => tool("spawn", json!({"name":"w","prompt":"make out/new.txt","write":["out"],"profile":"box"})),
            ("lead", _) => tool("wait", json!({})),
            (_, 0) => tool("write", json!({"path":"out/new.txt","content":"from worker\n"})),
            (_, 1) => tool("bash", json!({"command":"id -u; touch /should-be-private; touch notwritable.txt; echo rc=$?"})),
            _ => text("worker done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &[ "--max-workers", "2"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let l = json_lines(&o);
    assert!(l.iter().any(|x| x["type"] == "spawn" && x["agent"] == "w" && x["tier"] == 2), "spawn event carries tier 2");
    assert_eq!(std::fs::read_to_string(e.cwd.join("out/new.txt")).unwrap(), "from worker\n");
    let b = tool_outputs(&l, "w", "bash").concat();
    assert!(!b.contains("rc=0"), "worker cannot write outside its assigned path: {b}");
    assert!(!e.cwd.join("notwritable.txt").exists());
}

#[test]
fn missing_runtime_makes_spawn_fail_cleanly_while_the_lead_continues() {
    let e = Env::new("c_norun");
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("spawn", json!({"name":"w","prompt":"p","profile":"box"})),
        _ => text("lead continues"),
    });
    let cfg = json!({"container":{"runtime":"/nonexistent/crun"},"profiles":{"box":{"sandbox":"container"}}});
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2", "--sandbox", "none"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let s = tool_outputs(&json_lines(&o), "lead", "spawn");
    assert!(s[0].starts_with("Error:") && s[0].contains("not found"), "{s:?}");
    assert!(out(&o).contains("lead continues"));
}
