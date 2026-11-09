//! More tier 2 tests (isolation, network, lifecycle, mixed tiers). Same prerequisites and SKIP behavior as container.rs.
mod common;
mod ct;
use common::*;
use ct::*;
use serde_json::{Value, json};
use std::process::Command;

fn has_pasta() -> bool { Command::new("pasta").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) }

#[test]
fn root_fs_is_private_and_host_secrets_are_invisible() {
    let Some((e, cfg)) = setup("c_priv", json!({"profiles":{"box":{"sandbox":"container"}}})) else { return };
    let home = e.home.display().to_string();
    let dir = e.dir.display().to_string();
    let cfgfile = e.home.join("config/zcode/config.json").display().to_string();
    std::fs::create_dir_all(e.home.join("config/zcode")).unwrap();
    std::fs::write(&cfgfile, "SECRETCONFIG").unwrap();
    std::fs::create_dir_all(e.home.join(".ssh")).unwrap();
    std::fs::write(e.home.join(".ssh/id_rsa"), "SECRETKEY").unwrap();
    let llm = FakeLlm::start_fn(move |r| {
        let (w, all, n) = (who(r), all_text(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", _) if all.contains("[from b]") => text("fin"),
            ("lead", 0) => tool("spawn", json!({"name":"a","prompt":"mark","profile":"box"})),
            ("lead", 1) => tool("wait", json!({})),
            ("lead", 2) => tool("spawn", json!({"name":"b","prompt":"inspect","profile":"box"})),
            ("lead", _) => tool("wait", json!({})),
            ("a", 0) => tool("bash", json!({"command":"echo A > /private-mark; mkdir -p /opt/a; ls /private-mark"})),
            ("b", 0) => tool("bash", json!({"command": format!(
                "ls /private-mark 2>&1; echo mark_rc=$?; ls /opt/a 2>&1; echo opt_rc=$?; ls -A {dir}; echo dir_end; test -e {home}; echo home_rc=$?; cat {cfgfile} 2>&1; echo cfg_rc=$?; ls /root/.ssh 2>&1; env | cut -d= -f1 | grep -ci 'key\\|zcode\\|opencode\\|token'; echo env_end")})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "3"]).env("ZCODE_API_KEY", "sk-secret").output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let b = tool_outputs(&json_lines(&o), "b", "bash").concat();
    assert!(!b.contains("mark_rc=0") && b.contains("mark_rc=1"), "agent A's / change is invisible to B: {b}");
    assert!(b.contains("opt_rc=1"), "{b}");
    assert!(b.contains("work\ndir_end"), "only the project is visible next to it, not the host home: {b}");
    assert!(b.contains("home_rc=1"), "host $HOME invisible: {b}");
    assert!(!b.contains("SECRETCONFIG") && b.contains("cfg_rc=1"), "{b}");
    assert!(!b.contains("SECRETKEY"));
    assert!(b.contains("0\nenv_end"), "no key/zcode env inside the container: {b}");
    assert!(!out(&o).contains("sk-secret"));
}

#[test]
fn workers_cannot_touch_git_or_write_outside_assigned_paths() {
    let Some((e, cfg)) = setup("c_git", json!({"profiles":{"box":{"sandbox":"container"}}})) else { return };
    std::fs::create_dir_all(e.cwd.join(".git")).unwrap();
    std::fs::write(e.cwd.join(".git/HEAD"), "ref: refs/heads/main\n").unwrap();
    let llm = FakeLlm::start_fn(|r| {
        let (w, n) = (who(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", 0) => tool("spawn", json!({"name":"w","prompt":"try","write":["d1"],"profile":"box"})),
            ("lead", 1) => tool("wait", json!({})),
            ("lead", _) => text("fin"),
            (_, 0) => tool("bash", json!({"command":"echo x >> .git/HEAD; echo git_rc=$?; touch .git/new; echo gitnew_rc=$?; touch top.txt; echo top_rc=$?; touch d1/ok; echo ok_rc=$?"})),
            (_, 1) => tool("write", json!({"path":"d2.txt","content":"x"})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let l = json_lines(&o);
    let b = tool_outputs(&l, "w", "bash").concat();
    assert!(!b.contains("git_rc=0") && !b.contains("gitnew_rc=0") && !b.contains("top_rc=0") && b.contains("ok_rc=0"), "{b}");
    assert_eq!(std::fs::read_to_string(e.cwd.join(".git/HEAD")).unwrap(), "ref: refs/heads/main\n");
    assert!(tool_outputs(&l, "w", "write")[0].starts_with("Error:"));
    assert!(e.cwd.join("d1/ok").exists() && !e.cwd.join("top.txt").exists() && !e.cwd.join("d2.txt").exists());
}

#[test]
fn net_false_has_only_loopback() {
    let Some((e, cfg)) = setup("c_net", json!({"profiles":{"box":{"sandbox":"container"}}})) else { return };
    let llm = FakeLlm::start_fn(|r| {
        let (w, n) = (who(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", 0) => tool("spawn", json!({"name":"w","prompt":"net","profile":"box"})),
            ("lead", 1) => tool("wait", json!({})),
            ("lead", _) => text("fin"),
            (_, 0) => tool("bash", json!({"command":"awk -F: 'NR>2{print $1}' /proc/net/dev; echo ifaces_end; nc -w 2 93.184.216.34 80 </dev/null 2>&1; echo nc_rc=$?"})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let b = tool_outputs(&json_lines(&o), "w", "bash").concat();
    let ifaces = b.split("ifaces_end").next().unwrap().split_whitespace().collect::<Vec<_>>();
    assert_eq!(ifaces, vec!["lo"], "{b}");
    assert!(!b.contains("nc_rc=0"), "no outbound: {b}");
}

#[test]
fn two_net_agents_bind_the_same_port_and_get_different_host_ports() {
    if !has_pasta() { eprintln!("SKIP: pasta not installed"); return; }
    let Some((e, cfg)) = setup("c_ports", json!({"profiles":{"web":{"sandbox":"container","net":true,"ports":[3000]}}})) else { return };
    let llm = FakeLlm::start_fn(|r| {
        let (w, all, n) = (who(r), all_text(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", _) if all.contains("[from w1]") && all.contains("[from w2]") => text("fin"),
            ("lead", 0) => Script::Tools(vec![("spawn", json!({"name":"w1","prompt":"serve","profile":"web"}).to_string()), ("spawn", json!({"name":"w2","prompt":"serve","profile":"web"}).to_string())]),
            ("lead", _) => tool("wait", json!({})),
            (_, 0) => tool("bash", json!({"command":"(sleep 30 | nc -l -p 3000 >/dev/null 2>&1 &) ; sleep 1; cat /proc/net/tcp /proc/net/tcp6 | grep -ci ':0BB8'; echo listen_done"})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "3"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let l = json_lines(&o);
    for w in ["w1", "w2"] {
        assert!(tool_outputs(&l, w, "bash").concat().contains("1\nlisten_done"), "{w} listens on 3000: {:?}", tool_outputs(&l, w, "bash"));
    }
    let ports: Vec<u64> = l.iter().filter(|x| x["type"] == "port").map(|x| x["host"].as_u64().unwrap()).collect();
    assert_eq!(ports.len(), 2, "{ports:?}");
    assert_ne!(ports[0], ports[1]);
}

#[test]
fn killing_zcode_kills_containers_and_a_later_run_reaps_stale_state() {
    let Some((e, cfg)) = setup("c_kill", json!({})) else { return };
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("bash", json!({"command":"echo up"})),
        _ => Script::Stall(60),
    });
    let mut c = cmd(&e, &llm, cfg.clone(), &["--sandbox", "container"]);
    let mut child = c.stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap();
    let marker = format!("--cwd {}", e.cwd.display());
    let guests = |m: &str| -> usize {
        let o = Command::new("pgrep").args(["-f", &format!("zcode-guest.*{m}")]).output().unwrap();
        String::from_utf8_lossy(&o.stdout).lines().count()
    };
    let t = std::time::Instant::now();
    while guests(&marker) == 0 && t.elapsed().as_secs() < 20 { std::thread::sleep(std::time::Duration::from_millis(100)); }
    if guests(&marker) == 0 { child.kill().ok(); child.wait().ok(); eprintln!("SKIP: container did not start (tier 2 unavailable)"); return; }
    child.kill().unwrap();
    child.wait().unwrap();
    let t = std::time::Instant::now();
    while guests(&marker) > 0 && t.elapsed().as_secs() < 15 { std::thread::sleep(std::time::Duration::from_millis(100)); }
    assert_eq!(guests(&marker), 0, "killing zcode must kill its containers");
    // stale state of the dead run is still on disk until a new run reaps it
    let state = e.home.join("state/zcode");
    let before: Vec<_> = std::fs::read_dir(&state).unwrap().flatten().map(|d| d.file_name()).collect();
    assert!(!before.is_empty(), "dead run left state behind");
    let llm = FakeLlm::start(vec![tool("bash", json!({"command":"echo again"})), text("fin")]);
    let o = cmd(&e, &llm, cfg, &["--sandbox", "container"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    for d in &before { assert!(!state.join(d).exists(), "stale run dir {d:?} must be reaped"); }
}

#[test]
fn container_creation_does_not_block_the_agent_loop() {
    let Some((e, cfg)) = setup("c_async", json!({"profiles":{"box":{"sandbox":"container"}}})) else { return };
    let llm = FakeLlm::start_fn(|r| {
        let (w, n) = (who(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", 0) => tool("spawn", json!({"name":"w","prompt":"go","profile":"box"})),
            ("lead", 1) => tool("wait", json!({})),
            ("lead", _) => text("fin"),
            (_, 0) => tool("bash", json!({"command":"echo ready"})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}", err(&o));
    let l = json_lines(&o);
    let ts = |f: &dyn Fn(&Value) -> bool| l.iter().find(|x| f(x)).and_then(|x| x["timestamp"].as_u64()).unwrap();
    let spawn = ts(&|x| x["type"] == "spawn");
    let first_step = ts(&|x| x["type"] == "step_start" && x["agent"] == "w");
    let first_tool = ts(&|x| x["type"] == "tool_use" && x["agent"] == "w");
    assert!(first_step - spawn < 100, "worker's first model request starts at once ({} ms)", first_step - spawn);
    assert!(first_tool - first_step >= 30, "its first tool call waited for the container ({} ms)", first_tool - first_step);
}

#[test]
fn stale_edit_detection_across_the_host_guest_boundary_in_both_directions() {
    let Some((e, cfg)) = setup("c_stale", json!({"profiles":{"box":{"sandbox":"container"}}})) else { return };
    std::fs::write(e.cwd.join("f.txt"), "one\n").unwrap();
    std::fs::write(e.cwd.join("g.txt"), "alpha\n").unwrap();
    let llm = FakeLlm::start_fn(|r| {
        let (w, all, n) = (who(r), all_text(r), n_assist(r));
        match (w.as_str(), n) {
            // host reads f.txt, the container worker changes it, the host edit must fail
            ("lead", 0) => tool("read", json!({"path":"f.txt"})),
            ("lead", 1) => tool("spawn", json!({"name":"w","prompt":"edit f then read g and wait","write":["f.txt","g.txt"],"profile":"box"})),
            ("lead", 2) => tool("wait", json!({})),
            ("lead", 3) => tool("edit", json!({"path":"f.txt","old_string":"one","new_string":"lead"})),
            // opposite direction: the worker has read g.txt; the lead changes it; the worker edit must fail
            ("lead", 4) => tool("bash", json!({"command":"echo beta > g.txt"})),
            ("lead", 5) => tool("send", json!({"to":"w","text":"g changed"})),
            ("lead", 6) => tool("wait", json!({})),
            ("lead", 7) if !all.contains("[from w] worker finished") => tool("wait", json!({})),
            ("lead", _) => text("fin"),
            (_, 0) => tool("read", json!({"path":"f.txt"})),
            (_, 1) => tool("edit", json!({"path":"f.txt","old_string":"one","new_string":"worker"})),
            (_, 2) => tool("read", json!({"path":"g.txt"})),
            // tell the lead we are ready (so it is not idle), then park until it says g changed
            (_, 3) => tool("send", json!({"to":"lead","text":"ready"})),
            (_, 4) if all.contains("[from lead] g changed") => tool("edit", json!({"path":"g.txt","old_string":"alpha","new_string":"worker"})),
            (_, 4) => tool("wait", json!({})),
            (_, 5) => tool("edit", json!({"path":"g.txt","old_string":"alpha","new_string":"worker"})),
            _ => text("worker finished"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2", "--sandbox", "none"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let l = json_lines(&o);
    assert!(tool_outputs(&l, "lead", "edit").iter().any(|x| x.contains("modified by another agent")), "host side: {:?}", tool_outputs(&l, "lead", "edit"));
    assert!(tool_outputs(&l, "w", "edit").iter().any(|x| x.contains("modified by another agent")), "guest side: {:?}", tool_outputs(&l, "w", "edit"));
}

#[test]
fn a_net_false_tier1_lead_cannot_reach_a_workers_forwarded_port() {
    if !has_pasta() { eprintln!("SKIP: pasta not installed"); return; }
    let Some((e, cfg)) = setup("c_fwd", json!({"container":{"port_range":[41733,41733]},"profiles":{"web":{"sandbox":"container","net":true,"ports":[3000]}}})) else { return };
    let llm = FakeLlm::start_fn(|r| {
        let (w, n) = (who(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", 0) => tool("spawn", json!({"name":"w","prompt":"serve","profile":"web"})),
            ("lead", 1) => tool("wait", json!({})),
            ("lead", 2) => tool("bash", json!({"command":"bash -c 'exec 3<>/dev/tcp/127.0.0.1/41733' 2>&1; echo conn_rc=$?"})),
            ("lead", _) => text("fin"),
            (_, 0) => tool("bash", json!({"command":"(sleep 30 | nc -l -p 3000 >/dev/null 2>&1 &) ; sleep 1; echo serving"})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let b = tool_outputs(&json_lines(&o), "lead", "bash").concat();
    assert!(b.contains("not permitted") && !b.contains("conn_rc=0"), "AF_INET is denied for a net=false tier 1 lead: {b}");
}

fn has_slirp() -> bool { Command::new("slirp4netns").arg("--version").output().map(|o| o.status.success()).unwrap_or(false) }

/// pasta is made unavailable (`container.pasta` points nowhere), so `auto` must fall back to slirp4netns; a lead with `--net` reads from the forwarded port.
#[test]
fn slirp4netns_fallback_attaches_and_forwards_a_port() {
    if !has_slirp() { eprintln!("SKIP: slirp4netns not installed"); return; }
    let Some((e, cfg)) = setup("c_slirp", json!({"container":{"pasta":"/nonexistent/pasta","port_range":[41744,41744]},"profiles":{"web":{"sandbox":"container","net":true,"ports":[3000]}}})) else { return };
    let llm = FakeLlm::start_fn(|r| {
        let (w, n) = (who(r), n_assist(r));
        match (w.as_str(), n) {
            ("lead", 0) => tool("spawn", json!({"name":"w","prompt":"serve","profile":"web"})),
            ("lead", 1) => tool("wait", json!({})),
            ("lead", 2) => tool("bash", json!({"command":"bash -c 'exec 3<>/dev/tcp/127.0.0.1/41744; head -c 5 <&3' 2>&1; echo conn_rc=$?"})),
            ("lead", _) => text("fin"),
            (_, 0) => tool("bash", json!({"command":"(echo hello | nc -l -p 3000 >/dev/null 2>&1 &) ; sleep 1; grep -c tap0 /proc/net/dev; cat /etc/resolv.conf; echo serving"})),
            _ => text("done"),
        }
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2", "--net"]).output().unwrap();
    if skip_if_unavailable(&o) { return; }
    assert_eq!(code(&o), 0, "{}\n{}", err(&o), out(&o));
    let l = json_lines(&o);
    let w = tool_outputs(&l, "w", "bash").concat();
    assert!(w.contains("1\nnameserver 10.0.2.3"), "tap0 configured and slirp resolver mounted: {w}");
    assert!(l.iter().any(|x| x["type"] == "port" && x["host"] == 41744 && x["guest"] == 3000), "{l:?}");
    let b = tool_outputs(&l, "lead", "bash").concat();
    assert!(b.contains("hello") && b.contains("conn_rc=0"), "the forwarded port reaches the worker's server: {b}");
}

#[test]
fn a_missing_net_backend_fails_spawn_cleanly_and_the_lead_continues() {
    let Some((e, cfg)) = setup("c_nonet", json!({"container":{"pasta":"/nonexistent/pasta","slirp4netns":"/nonexistent/slirp","net_backend":"auto"},"profiles":{"web":{"sandbox":"container","net":true}}})) else { return };
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("spawn", json!({"name":"w","prompt":"x","profile":"web"})),
        _ => text("lead continues"),
    });
    let o = cmd(&e, &llm, cfg, &["--max-workers", "2"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let s = tool_outputs(&json_lines(&o), "lead", "spawn").concat();
    assert!(s.contains("passt") && s.contains("slirp4netns") && s.contains("use net=false"), "{s}");
}
