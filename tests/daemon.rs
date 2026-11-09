mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};

fn text(s: &str) -> Script { Script::Text(s.into()) }
fn tool(name: &'static str, args: Value) -> Script { Script::Tools(vec![(name, args.to_string())]) }
fn first_user(r: &Value) -> String { r["messages"][1]["content"].as_str().unwrap_or("").to_string() }
fn n_assist(r: &Value) -> usize { r["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").count() }
fn all_text(r: &Value) -> String {
    r["messages"].as_array().unwrap().iter().skip(1).map(|m| format!("{}|{}", m["role"], m["content"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")
}

fn turn_end(inst: &'static str) -> impl Fn(&Value) -> bool { move |e| e["type"] == "turn_end" && e["instance"] == inst }

#[test]
fn two_clients_create_attach_and_detach_independent_instances() {
    let e = Env::new("d_two");
    let other = e.dir.join("other");
    std::fs::create_dir_all(&other).unwrap();
    let llm = FakeLlm::start_fn(|r| if first_user(r).contains("alpha") { text("answer-alpha") } else { text("answer-beta") });
    let d = start(&e, json!({}), &[]);
    let (mut a, mut b) = (Client::connect(&d), Client::connect(&d));
    create(&mut a, "a1", &e.cwd, &llm, json!({}));
    create(&mut b, "b1", &other, &llm, json!({}));
    a.ok("attach", json!({"instance": "a1"}));
    b.ok("attach", json!({"instance": "b1"}));
    a.ok("prompt", json!({"instance": "a1", "text": "alpha"}));
    b.ok("prompt", json!({"instance": "b1", "text": "beta"}));
    let ea = a.wait("a1 end", turn_end("a1"));
    let eb = b.wait("b1 end", turn_end("b1"));
    assert_eq!((ea["code"].as_i64(), ea["text"].as_str()), (Some(0), Some("answer-alpha")));
    assert_eq!(eb["text"], "answer-beta");
    // events carry instance + agent; neither client saw the other's events
    assert!(a.events.iter().all(|x| x["instance"] == "a1") && b.events.iter().all(|x| x["instance"] == "b1"));
    assert!(a.events.iter().any(|x| x["type"] == "text" && x["agent"] == "a1"));
    // detach, then re-attach by id: the ring replays what happened while detached
    a.ok("detach", json!({"instance": "a1"}));
    a.events.clear();
    a.ok("prompt", json!({"instance": "a1", "text": "alpha again"}));
    std::thread::sleep(std::time::Duration::from_millis(300));
    let r = a.ok("attach", json!({"instance": "a1", "since": 0}));
    assert!(r["replayed"].as_u64().unwrap() >= 6, "{r}");
    a.wait("replayed first turn end", turn_end("a1"));
    a.wait("replayed second turn end", turn_end("a1"));
    // status lists only this app's instances, with their state
    let s = a.ok("status", json!({}));
    assert_eq!(s["instances"].as_array().unwrap().len(), 2);
    a.ok("close", json!({"instance": "a1"}));
    assert_eq!(a.call("prompt", json!({"instance": "a1", "text": "x"}))["ok"], false);
    // the daemon never used its own credentials or config and never echoed the client's key
    let all = serde_json::to_string(&[&a.events, &b.events]).unwrap();
    assert!(!all.contains("sk-client-secret"));
    // the sessions were written by the daemon (its own HOME), not by the clients
    assert!(d.home.join("data/zcode/sessions").read_dir().unwrap().count() >= 2);
}

#[test]
fn a_dead_client_does_not_kill_its_instances() {
    let e = Env::new("d_dead");
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("bash", json!({"command": "sleep 1; echo survived"})),
        _ => text("finished"),
    });
    let d = start(&e, json!({}), &[]);
    {
        let mut a = Client::connect(&d);
        create(&mut a, "keep", &e.cwd, &llm, json!({}));
        a.ok("attach", json!({"instance": "keep"}));
        a.ok("prompt", json!({"instance": "keep", "text": "go"}));
        a.wait("first tool", |x| x["type"] == "step_start");
    } // connection dropped mid-turn
    std::thread::sleep(std::time::Duration::from_millis(2500));
    let mut b = Client::connect(&d);
    let s = b.ok("status", json!({"instance": "keep"}));
    assert_eq!(s["instances"][0]["running"], false);
    b.ok("attach", json!({"instance": "keep"}));
    let end = b.wait("turn_end replayed", turn_end("keep"));
    assert_eq!(end["text"], "finished");
    assert!(b.events.iter().chain(std::iter::once(&end)).count() > 0);
    assert!(b.events.iter().any(|x| x["type"] == "tool_use" && x["part"]["state"]["output"].as_str().unwrap_or("").contains("survived")));
}

#[test]
fn groups_share_a_bus_and_other_groups_are_isolated() {
    let e = Env::new("d_group");
    let llm = FakeLlm::start_fn(|r| {
        let (all, n) = (all_text(r), n_assist(r));
        let me = if r["messages"][0]["content"].as_str().unwrap_or("").contains("cwd:") { first_user(r) } else { String::new() };
        match (me.as_str(), n) {
            ("to-g1b", 0) => tool("send", json!({"to":"g1b","text":"hello across the bus"})),
            ("to-g1b", 1) => tool("send", json!({"to":"g2a","text":"can you hear me"})),
            ("to-g1b", _) => text("sent both"),
            _ if all.contains("[from g1a] hello across the bus") => text("g1b got it"),
            _ => text("idle"),
        }
    });
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    for (n, g) in [("g1a", "g1"), ("g1b", "g1"), ("g2a", "g2")] { create(&mut c, n, &e.cwd, &llm, json!({"group": g})); c.ok("attach", json!({"instance": n})); }
    c.ok("prompt", json!({"instance": "g1b", "text": "idle-start"})); // build g1b so it can receive
    c.wait("g1b ready", turn_end("g1b"));
    c.ok("prompt", json!({"instance": "g2a", "text": "idle-start"}));
    c.wait("g2a ready", turn_end("g2a"));
    c.ok("prompt", json!({"instance": "g1a", "text": "to-g1b"}));
    c.wait("g1a done", turn_end("g1a"));
    // the message arrived in g1b, which was idle: it woke up on its own and answered
    let woke = c.wait("g1b woke", |x| x["type"] == "turn_end" && x["instance"] == "g1b" && x["text"] == "g1b got it");
    assert_eq!(woke["code"], 0);
    // g2a is in another group: the send failed and nothing reached it
    let sends: Vec<String> = c.events.iter().filter(|x| x["type"] == "tool_use" && x["agent"] == "g1a").map(|x| x["part"]["state"]["output"].as_str().unwrap_or("").to_string()).collect();
    assert!(sends.iter().any(|s| s.contains("sent to g1b")), "{sends:?}");
    assert!(sends.iter().any(|s| s.starts_with("Error:") && s.contains("no such agent")), "{sends:?}");
    assert!(!llm.requests.lock().unwrap().iter().any(|r| all_text(r).contains("can you hear me")));
}

#[test]
fn tools_never_inherit_the_daemon_environment_and_apps_are_namespaced() {
    let e = Env::new("d_env");
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("bash", json!({"command": "env | cut -d= -f1 | grep -c 'DAEMON_SECRET\\|ZCODE'; echo end"})),
        _ => text("ok"),
    });
    let cfg = json!({"serve": {"apps": {"alice": {"token": "ta"}, "bob": {"token": "tb"}}}});
    let d = start(&e, cfg, &[("DAEMON_SECRET_KEY", "daemon-only")]);
    let mut anon = Client::connect(&d);
    assert_eq!(anon.call("status", json!({}))["ok"], false, "hello required when apps are configured");
    let mut a = Client::connect(&d);
    assert_eq!(a.call("hello", json!({"app": "alice", "token": "wrong"}))["ok"], false);
    a.ok("hello", json!({"app": "alice", "token": "ta"}));
    create(&mut a, "same", &e.cwd, &llm, json!({}));
    a.ok("attach", json!({"instance": "same"}));
    a.ok("prompt", json!({"instance": "same", "text": "go"}));
    a.wait("end", turn_end("same"));
    let out = a.events.iter().find(|x| x["type"] == "tool_use").unwrap()["part"]["state"]["output"].as_str().unwrap().to_string();
    assert!(out.starts_with("0\n"), "no daemon env inside tools: {out}");
    // bob cannot see alice's instance, and can create his own with the same name
    let mut b = Client::connect(&d);
    b.ok("hello", json!({"app": "bob", "token": "tb"}));
    assert_eq!(b.call("status", json!({"instance": "same"}))["ok"], false);
    create(&mut b, "same", &e.cwd, &llm, json!({}));
    assert_eq!(b.ok("status", json!({}))["instances"].as_array().unwrap().len(), 1);
}

#[test]
fn caps_cancel_and_creation_is_lazy() {
    let e = Env::new("d_caps");
    let llm = FakeLlm::start_fn(|_| Script::Stall(30));
    let d = start(&e, json!({"serve": {"max_instances": 2}}), &[]);
    let mut c = Client::connect(&d);
    let t = std::time::Instant::now();
    create(&mut c, "i1", &e.cwd, &llm, json!({}));
    create(&mut c, "i2", &e.cwd, &llm, json!({}));
    assert!(t.elapsed().as_millis() < 500, "create is allocation only");
    assert_eq!(llm.count(), 0, "nothing was built or requested at create time");
    assert!(d.home.join("data/zcode/sessions").read_dir().map(|d| d.count()).unwrap_or(0) == 0, "session file is lazy");
    let third = c.call("create", json!({"instance": "i3", "cwd": e.cwd, "config": client_config(&llm, "k"), "sandbox": "none"}));
    assert_eq!(third["ok"], false);
    assert!(third["err"].as_str().unwrap().contains("limit"));
    // bad requests are refused, not crashed on
    assert_eq!(c.call("create", json!({"instance": "../x", "cwd": e.cwd, "config": {}}))["ok"], false);
    assert_eq!(c.call("create", json!({"instance": "n", "cwd": "relative", "config": {}}))["ok"], false);
    assert_eq!(c.call("nope", json!({}))["ok"], false);
    c.raw_send(&json!({"not": "a request"}));
    assert_eq!(c.call("status", json!({}))["ok"], true, "daemon still alive after a malformed request");
    // cancel a turn stuck on the model
    c.ok("attach", json!({"instance": "i1"}));
    c.ok("prompt", json!({"instance": "i1", "text": "hang"}));
    c.wait("started", |x| x["type"] == "step_start");
    assert_eq!(c.call("prompt", json!({"instance": "i1", "text": "again"}))["ok"], false, "busy");
    c.ok("cancel", json!({"instance": "i1"}));
    let end = c.wait("turn_end", turn_end("i1"));
    assert_eq!((end["stop"].as_str(), end["code"].as_i64()), (Some("interrupted"), Some(130)));
}

fn normalized(o: &std::process::Output) -> Vec<(String, String)> {
    json_lines(o).iter().map(|l| (l["type"].as_str().unwrap_or("").to_string(), l["part"]["text"].as_str().unwrap_or("").to_string())).collect()
}

#[test]
fn zcode_run_uses_a_running_daemon_and_falls_back_when_there_is_none() {
    let e = Env::new("d_run");
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("write", json!({"path": "made.txt", "content": "hi"})),
        _ => text("all done"),
    });
    // 1) no daemon: embedded
    let run = |e: &Env, llm: &FakeLlm, fmt: &str| {
        let mut c = e.cmd(llm, &["run", "--auto", "--sandbox", "none", "--format", fmt, "go"]);
        c.env("XDG_RUNTIME_DIR", e.dir.join("runtime")).env("XDG_STATE_HOME", e.home.join("state"));
        std::fs::create_dir_all(e.dir.join("runtime")).unwrap();
        c.output().unwrap()
    };
    let embedded = run(&e, &llm, "json");
    assert_eq!(code(&embedded), 0, "{}", err(&embedded));
    assert!(e.cwd.join("made.txt").exists());
    assert_eq!(e.sessions().len(), 1, "embedded run wrote the client's session");
    std::fs::remove_file(e.cwd.join("made.txt")).unwrap();
    // 2) with a daemon: same output, but the work happens in the daemon (its HOME holds the session)
    let d = start(&e, json!({}), &[]);
    let via = run(&e, &llm, "json");
    assert_eq!(code(&via), 0, "{}", err(&via));
    assert_eq!(normalized(&via), normalized(&embedded), "identical --format json stream");
    assert!(e.cwd.join("made.txt").exists());
    assert_eq!(e.sessions().len(), 1, "client wrote no new session");
    assert!(d.home.join("data/zcode/sessions").read_dir().unwrap().count() >= 1, "the daemon ran it");
    let human = run(&e, &llm, "default");
    assert_eq!(code(&human), 0, "{}", err(&human));
    assert!(out(&human).contains("all done"), "{}", out(&human));
    // 3) the daemon's exit codes carry over (limit = 3)
    let loop_llm = FakeLlm::start_with_default(vec![], Some(tool("bash", json!({"command": "echo again"}))));
    let mut c = e.cmd(&loop_llm, &["run", "--auto", "--sandbox", "none", "--max-steps", "3", "go"]);
    c.env("XDG_RUNTIME_DIR", e.dir.join("runtime"));
    assert_eq!(code(&c.output().unwrap()), 3);
    // 4) daemon gone: falls back to embedded again with identical behavior
    drop(d);
    let _ = std::fs::remove_file(e.dir.join("runtime/zcode/zcode.sock"));
    std::fs::remove_file(e.cwd.join("made.txt")).unwrap();
    let after = run(&e, &llm, "json");
    assert_eq!(code(&after), 0);
    assert_eq!(normalized(&after), normalized(&embedded));
    assert_eq!(e.sessions().len(), 2, "embedded again");
}

#[test]
fn a_panic_in_one_instance_does_not_take_down_the_daemon() {
    let e = Env::new("d_panic");
    let llm = FakeLlm::start_fn(|_| text("fine"));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "bad", &e.cwd, &llm, json!({}));
    create(&mut c, "good", &e.cwd, &llm, json!({}));
    c.ok("attach", json!({"instance": "bad"}));
    c.ok("attach", json!({"instance": "good"}));
    c.ok("prompt", json!({"instance": "bad", "text": "__zcode_test_panic__"}));
    let end = c.wait("bad turn_end", turn_end("bad"));
    assert_eq!((end["stop"].as_str(), end["code"].as_i64()), (Some("error"), Some(1)));
    assert!(c.events.iter().any(|x| x["type"] == "error" && x["error"]["type"] == "panic"));
    std::thread::sleep(std::time::Duration::from_millis(200));
    let again = c.call("prompt", json!({"instance": "bad", "text": "x"}));
    assert_eq!(again["ok"], false);
    assert!(again["err"].as_str().unwrap().contains("crashed"));
    c.ok("prompt", json!({"instance": "good", "text": "hello"}));
    assert_eq!(c.wait("good turn_end", turn_end("good"))["text"], "fine");
}

#[test]
fn a_group_builds_on_demand_keeps_going_past_the_swarm_budget_and_a_person_re_arms_the_message_caps() {
    let e = Env::new("d_peer_caps");
    let llm = FakeLlm::start_fn(|_| text("ok"));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    // tiny per-turn limits and a pair cap of 2: a group is long-lived, so only the per-turn limits and the cap on agent chatter apply
    create(&mut c, "a", &e.cwd, &llm, json!({"group": "g", "limits": {"max_steps": 2}, "swarm": {"pair_cap": 2}}));
    c.ok("attach", json!({"instance": "a"}));
    // `build` joins the group now (nothing was built at create) and lists who is in it
    let b = c.ok("build", json!({"instance": "a"}));
    assert!(b["agents"].as_array().unwrap().iter().any(|l| l.as_str().unwrap().starts_with("a ")), "{b}");
    // far more steps over time than max_steps (2): the swarm as a whole is not budgeted
    for i in 0..4 {
        c.ok("prompt", json!({"instance": "a", "text": format!("turn {i}")}));
        let t = c.wait("turn end", turn_end("a"));
        assert_eq!(t["code"], 0, "turn {i}: {t}");
    }
    // messages to it count against the pair cap (2) ...
    for i in 0..2 {
        c.ok("send", json!({"instance": "a", "to": "a", "text": format!("ping {i}")}));
        c.wait("woken by the message", turn_end("a"));
    }
    let r = c.call("send", json!({"instance": "a", "to": "a", "text": "ping 2"}));
    assert_eq!(r["ok"], false);
    assert!(r["err"].as_str().unwrap().contains("hop limit"), "{r}");
    // ... until a person types: then the agents may talk again
    c.ok("prompt", json!({"instance": "a", "text": "I am here"}));
    c.wait("turn end", turn_end("a"));
    c.ok("send", json!({"instance": "a", "to": "a", "text": "ping 3"}));
    c.wait("woken again", turn_end("a"));
}

#[test]
fn an_instance_created_with_close_on_disconnect_ends_with_its_connection() {
    let e = Env::new("d_ephemeral");
    let llm = FakeLlm::start_fn(|_| text("ok"));
    let d = start(&e, json!({}), &[]);
    let mut watcher = Client::connect(&d);
    {
        let mut c = Client::connect(&d);
        create(&mut c, "mine", &e.cwd, &llm, json!({"close_on_disconnect": true}));
        create(&mut c, "kept", &e.cwd, &llm, json!({}));
        assert_eq!(watcher.ok("status", json!({}))["instances"].as_array().unwrap().len(), 2);
    }
    let t = std::time::Instant::now();
    loop {
        let s = watcher.ok("status", json!({}));
        let names: Vec<&str> = s["instances"].as_array().unwrap().iter().map(|i| i["instance"].as_str().unwrap()).collect();
        if names == ["kept"] { break; }
        assert!(t.elapsed() < std::time::Duration::from_secs(10), "only the flagged instance should end: {names:?}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn exec_runs_a_command_in_the_instances_sandbox_and_returns_its_output() {
    let e = Env::new("d_exec");
    let llm = FakeLlm::start_fn(|_| text("unused"));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "x1", &e.cwd, &llm, json!({}));
    let r = c.ok("exec", json!({"instance": "x1", "command": "echo hello; exit 3"}));
    assert_eq!(r["code"], 3, "{r}");
    assert!(r["output"].as_str().unwrap().contains("hello"), "{r}");
    assert_eq!(llm.count(), 0, "exec never calls the model");
    assert_eq!(c.call("exec", json!({"instance": "nope", "command": "true"}))["ok"], false);
    assert_eq!(c.call("exec", json!({"instance": "x1"}))["ok"], false);
}

#[test]
fn an_instance_can_share_another_instances_sandbox() {
    let e = Env::new("d_shared");
    let llm = FakeLlm::start_fn(|_| text("unused"));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "base", &e.cwd, &llm, json!({"group": "g"}));
    create(&mut c, "guest", &e.cwd, &llm, json!({"sandbox_of": "base", "group": "g"}));
    c.ok("build", json!({"instance": "base"}));
    c.ok("build", json!({"instance": "guest"}));
    c.ok("exec", json!({"instance": "base", "command": "cd /usr"}));
    let r = c.ok("exec", json!({"instance": "guest", "command": "pwd"}));
    assert!(r["output"].as_str().unwrap().contains("/usr"), "{r}");
    c.ok("close", json!({"instance": "guest"}));
    let r = c.ok("exec", json!({"instance": "base", "command": "echo still-open"}));
    assert!(r["output"].as_str().unwrap().contains("still-open"), "{r}");
    assert_eq!(c.call("create", json!({"instance": "x", "cwd": e.cwd, "config": {}, "sandbox_of": "nope"}))["ok"], false);
}

#[test]
fn attach_uses_an_instance_another_client_created_and_leaving_only_detaches() {
    use std::io::Write;
    use std::process::Stdio;
    let e = Env::new("d_attach");
    let llm = FakeLlm::start_fn(|_| text("hello from the attached agent"));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "worker1", &e.cwd, &llm, json!({"group": "g"}));
    // the attaching process has NO model config at all: the instance carries its own
    let mut cmd = std::process::Command::new(env!("CARGO_BIN_EXE_zcode"));
    cmd.args(["--attach", "worker1"]).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default()).env("HOME", e.dir.join("nohome"))
        .env("XDG_RUNTIME_DIR", e.dir.join("runtime")).env("NO_COLOR", "1")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    child.stdin.take().unwrap().write_all(b"hi there\n").unwrap(); // closing stdin = end of piped input: leaves when idle
    let o = child.wait_with_output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let so = out(&o);
    assert!(so.contains("attached to `worker1`"), "{so}");
    assert!(so.contains("hello from the attached agent"), "{so}");
    // it was only detached: the instance (and its session) is still there
    let s = c.ok("status", json!({"instance": "worker1"}));
    assert_eq!(s["instances"][0]["instance"], "worker1");
    assert!(s["instances"][0]["events"].as_u64().unwrap() > 0, "{s}");
    // attaching to something that does not exist is a clear error, not a new agent
    let o = std::process::Command::new(env!("CARGO_BIN_EXE_zcode")).args(["--attach", "nope"]).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default())
        .env("XDG_RUNTIME_DIR", e.dir.join("runtime")).output().unwrap();
    assert_ne!(code(&o), 0);
    assert!(err(&o).contains("no such instance"), "{}", err(&o));
}

#[test]
fn thinking_is_off_by_default_and_can_be_switched_on_for_a_running_instance() {
    let e = Env::new("d_think");
    let llm = FakeLlm::start_fn(|_| Script::Reasoning("pondering the question".into(), "the answer".into()));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "t1", &e.cwd, &llm, json!({"stream": true}));
    c.ok("attach", json!({"instance": "t1"}));
    assert_eq!(c.ok("thinking", json!({"instance": "t1"}))["thinking"], false);
    c.ok("prompt", json!({"instance": "t1", "text": "one"}));
    c.wait("first end", turn_end("t1"));
    assert!(!c.events.iter().any(|x| x["type"] == "reasoning_delta"), "no reasoning while thinking is off");
    assert_eq!(c.ok("thinking", json!({"instance": "t1", "on": true}))["thinking"], true);
    assert_eq!(c.ok("status", json!({"instance": "t1"}))["instances"][0]["thinking"], true);
    c.ok("prompt", json!({"instance": "t1", "text": "two"}));
    c.wait("second end", |e| e["type"] == "turn_end" && e["instance"] == "t1" && e["seq"].as_u64().unwrap_or(0) > 10);
    let r: String = c.events.iter().filter(|x| x["type"] == "reasoning_delta").map(|x| x["delta"].as_str().unwrap_or("")).collect();
    assert!(r.contains("pondering the question"), "{r:?}");
    // created with thinking on from the start
    create(&mut c, "t2", &e.cwd, &llm, json!({"stream": true, "thinking": true}));
    assert_eq!(c.ok("thinking", json!({"instance": "t2"}))["thinking"], true);
}

#[test]
fn attach_shows_the_reasoning_and_slash_thinking_toggles_it() {
    use std::io::Write;
    use std::process::Stdio;
    let e = Env::new("d_attach_think");
    let llm = FakeLlm::start_fn(|_| Script::Reasoning("weighing the options".into(), "final answer".into()));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "w1", &e.cwd, &llm, json!({"stream": true, "thinking": true}));
    let attach = |input: &str| {
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_zcode")).args(["--attach", "w1"]).env_clear().env("PATH", std::env::var("PATH").unwrap_or_default())
            .env("XDG_RUNTIME_DIR", e.dir.join("runtime")).env("NO_COLOR", "1").stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        out(&child.wait_with_output().unwrap())
    };
    let shown = attach("question\n");
    assert!(shown.contains("weighing the options") && shown.contains("final answer"), "{shown}");
    let off = attach("/thinking off\nquestion\n");
    assert!(off.contains("thinking off") && off.contains("final answer") && !off.contains("weighing the options"), "{off}");
    assert_eq!(c.ok("thinking", json!({"instance": "w1"}))["thinking"], false, "the setting stays on the instance");
    let bad = attach("/thinking maybe\n");
    assert!(bad.contains("usage: /thinking"), "{bad}");
}
