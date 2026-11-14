//! Daemon protocol additions used by external UIs: `user`, `tool_start` pairing, timestamps, `status`, `history`, `cancel`, `catch_up`.
mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};

fn text(s: &str) -> Script { Script::Text(s.into()) }
fn tool(name: &'static str, args: Value) -> Script { Script::Tools(vec![(name, args.to_string())]) }
fn n_assist(r: &Value) -> usize { r["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").count() }
fn turn_end(inst: &'static str) -> impl Fn(&Value) -> bool { move |e| e["type"] == "turn_end" && e["instance"] == inst }

#[test]
fn a_prompt_produces_a_user_event_before_the_first_step() {
    let e = Env::new("ev_user");
    let llm = FakeLlm::start_fn(|_| text("hi there"));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "u", &e.cwd, &llm, json!({}));
    c.ok("attach", json!({"instance": "u"}));
    c.ok("prompt", json!({"instance": "u", "text": "hello agent"}));
    c.wait("turn end", turn_end("u"));
    let kinds: Vec<&str> = c.events.iter().filter_map(|x| x["type"].as_str()).collect();
    let (iu, is) = (kinds.iter().position(|k| *k == "user"), kinds.iter().position(|k| *k == "step_start"));
    assert!(iu.is_some() && iu < is, "{kinds:?}");
    let u = c.events.iter().find(|x| x["type"] == "user").unwrap();
    assert_eq!((u["text"].as_str(), u["source"].as_str(), u["agent"].as_str()), (Some("hello agent"), Some("client"), Some("u")));
    // a late attacher sees it in the replayed ring
    let mut late = Client::connect(&d);
    late.ok("attach", json!({"instance": "u", "since": 0}));
    late.wait("replayed user event", |x| x["type"] == "user" && x["text"] == "hello agent");
}

#[test]
fn thin_clients_do_not_print_daemon_only_events() {
    let e = Env::new("ev_thin");
    let llm = FakeLlm::start_fn(|_| text("plain answer"));
    let d = start(&e, json!({}), &[]);
    let _ = &d;
    let out = e.cmd(&llm, &["run", "--auto", "--format", "json", "say hi"])
        .env("XDG_RUNTIME_DIR", d.sock.parent().unwrap().parent().unwrap()).output().unwrap();
    let lines = json_lines(&out);
    assert!(!lines.is_empty(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(lines.iter().all(|l| !matches!(l["type"].as_str(), Some("user" | "partial" | "tool_start"))), "{lines:?}");
    assert!(lines.iter().any(|l| l["type"] == "text"));
    assert!(d.home.join("data/zcode/sessions").read_dir().unwrap().count() >= 1, "the daemon ran it");
}

fn tool_pairs(events: &[Value], agent: &str) -> Vec<(String, Value, Value)> {
    let starts: Vec<&Value> = events.iter().filter(|x| x["type"] == "tool_start" && x["agent"] == agent).collect();
    starts.iter().map(|s| {
        let id = s["callID"].as_str().expect("tool_start has a callID").to_string();
        let end = events.iter().find(|x| x["type"] == "tool_use" && x["part"]["callID"] == id.as_str()).unwrap_or_else(|| panic!("no tool_use for {id}"));
        (id, (*s).clone(), end.clone())
    }).collect()
}

#[test]
fn tool_start_carries_the_call_id_and_input_so_parallel_calls_pair_exactly() {
    let e = Env::new("ev_pair");
    std::fs::write(e.cwd.join("a.txt"), "alpha").unwrap();
    std::fs::write(e.cwd.join("b.txt"), "beta").unwrap();
    let big = "x".repeat(40_000);
    let llm = FakeLlm::start_fn(move |r| match n_assist(r) {
        0 => Script::Tools(vec![("read", json!({"path": "a.txt"}).to_string()), ("read", json!({"path": "b.txt"}).to_string()), ("grep", json!({"pattern": big}).to_string())]),
        _ => text("done"),
    });
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "p", &e.cwd, &llm, json!({}));
    c.ok("attach", json!({"instance": "p"}));
    c.ok("prompt", json!({"instance": "p", "text": "read both"}));
    c.wait("turn end", turn_end("p"));
    let pairs = tool_pairs(&c.events, "p");
    assert_eq!(pairs.len(), 3, "{:?}", c.events);
    let ids: std::collections::HashSet<&String> = pairs.iter().map(|p| &p.0).collect();
    assert_eq!(ids.len(), 3, "call ids are distinct");
    for (_, s, end) in &pairs {
        assert_eq!(s["tool"], end["part"]["tool"]);
        // the parsed input object, the same one the tool_use reports (unless it was huge and got capped)
        if s["tool"] != "grep" { assert_eq!(s["input"], end["part"]["state"]["input"]); }
    }
    let by_path = |p: &str| pairs.iter().find(|(_, s, _)| s["input"]["path"] == p).map(|x| x.2["part"]["state"]["output"].as_str().unwrap().to_string()).unwrap();
    assert!(by_path("a.txt").contains("alpha") && by_path("b.txt").contains("beta"));
    let grep = pairs.iter().find(|(_, s, _)| s["tool"] == "grep").unwrap();
    assert!(grep.1["input"]["pattern"].as_str().unwrap().len() < 20_000, "huge input is capped");
}

#[test]
fn every_event_frame_has_a_timestamp() {
    let e = Env::new("ev_ts");
    let llm = FakeLlm::start_fn(|r| {
        let all = r["messages"].to_string();
        if all.contains("ping") { text("got the message") } else { Script::Reasoning("pondering".into(), "answer".into()) }
    });
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "t1", &e.cwd, &llm, json!({"group": "g", "stream": true, "thinking": true}));
    c.ok("attach", json!({"instance": "t1"}));
    let mut obs = Client::connect(&d); // `close` stops the closing connection's own forwarder, so another client sees `closed`
    obs.ok("attach", json!({"instance": "t1"}));
    c.ok("prompt", json!({"instance": "t1", "text": "think about it"}));
    let mut waited = vec![c.wait("first turn", turn_end("t1"))];
    c.ok("send", json!({"instance": "t1", "to": "t1", "text": "ping"}));
    waited.push(c.wait("woken turn", |x| x["type"] == "turn_end" && x["text"] == "got the message"));
    c.ok("close", json!({"instance": "t1"}));
    waited.push(obs.wait("closed", |x| x["type"] == "closed"));
    let all: Vec<&Value> = c.events.iter().chain(obs.events.iter()).chain(waited.iter()).collect();
    let kinds: std::collections::HashSet<&str> = all.iter().filter_map(|x| x["type"].as_str()).collect();
    for k in ["text_delta", "reasoning_delta", "turn_end", "step_start", "closed", "user", "incoming"] { assert!(kinds.contains(k), "{k} missing in {kinds:?}"); }
    for ev in all { assert!(ev["timestamp"].as_u64().is_some_and(|t| t > 1_600_000_000_000), "no timestamp on {ev}"); }
}

fn status_of(c: &mut Client, name: &str) -> Value { c.ok("status", json!({"instance": name}))["instances"][0].clone() }

#[test]
fn status_reports_the_turn_in_flight_the_queue_and_the_last_event() {
    let e = Env::new("ev_status");
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("bash", json!({"command": "sleep 2; echo slept"})),
        _ => text("finished"),
    });
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "s", &e.cwd, &llm, json!({"group": "g"}));
    c.ok("build", json!({"instance": "s"}));
    // idle: no turn, nothing queued, no events yet except the user/restored ones that do not exist
    let idle = status_of(&mut c, "s");
    assert!(idle["turn"].is_null() && idle["queued"] == 0 && idle["running"] == false, "{idle}");
    assert!(idle["last_event_ts"].is_null(), "{idle}");
    let before = common_now_ms();
    c.ok("prompt", json!({"instance": "s", "text": "work"}));
    // wait for the tool to be executing
    let t = std::time::Instant::now();
    let busy = loop {
        let s = status_of(&mut c, "s");
        if s["turn"]["tool"] == "bash" { break s; }
        assert!(t.elapsed().as_secs() < 10, "never saw the tool: {s}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(busy["running"], true);
    assert!(busy["turn"]["since"].as_u64().unwrap() >= before && busy["turn"]["step"].as_u64().unwrap() >= 1, "{busy}");
    // messages sent to the busy instance wait until its next step
    c.ok("send", json!({"instance": "s", "to": "s", "text": "one"}));
    c.ok("send", json!({"instance": "s", "to": "s", "text": "two"}));
    let q = status_of(&mut c, "s");
    assert_eq!(q["queued"], 2, "{q}");
    assert!(q["last_event_ts"].as_u64().unwrap() >= busy["turn"]["since"].as_u64().unwrap(), "{q}");
    c.ok("attach", json!({"instance": "s", "since": 0}));
    c.wait("turn end", turn_end("s"));
    let t = std::time::Instant::now();
    let done = loop {
        let s = status_of(&mut c, "s");
        if s["turn"].is_null() && s["running"] == false { break s; }
        assert!(t.elapsed().as_secs() < 10, "{s}");
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    assert_eq!(done["queued"], 0, "{done}");
    // the existing fields are still there
    for k in ["instance", "cwd", "group", "running", "crashed", "built", "events", "thinking", "pid", "ports", "agents"] { assert!(done.get(k).is_some(), "{k} missing: {done}"); }
}

fn common_now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis() as u64 }

fn sigkill(d: &mut Daemon) { let _ = d.child.kill(); let _ = d.child.wait(); }

#[test]
fn history_reads_an_unbuilt_instances_session_and_pages_backwards() {
    let e = Env::new("ev_hist_seed");
    let llm = FakeLlm::start_fn(|_| text("unused"));
    let d = start(&e, json!({}), &[]);
    let dir = d.home.join("data/zcode/sessions");
    std::fs::create_dir_all(&dir).unwrap();
    let mut lines = vec![json!({"zcode_session": {"id": "ses_seed", "cwd": e.cwd, "model": "m", "created": 1}}).to_string()];
    for i in 0..10 { lines.push(json!({"role": if i % 2 == 0 { "user" } else { "assistant" }, "content": format!("m{i}")}).to_string()); }
    std::fs::write(dir.join("ses_seed.jsonl"), lines.join("\n") + "\n").unwrap();
    let mut c = Client::connect(&d);
    create(&mut c, "h", &e.cwd, &llm, json!({"session": "ses_seed"}));
    // nothing was built: the answer comes from the file alone, and building was not needed
    let all = c.ok("history", json!({"instance": "h"}));
    assert_eq!((all["total"].as_u64(), all["first"].as_u64()), (Some(10), Some(0)));
    assert_eq!(all["items"][0], json!({"k": "user", "text": "m0"}));
    assert_eq!(all["items"][1], json!({"k": "text", "text": "m1"}));
    assert_eq!(c.ok("status", json!({"instance": "h"}))["instances"][0]["built"], false);
    let last = c.ok("history", json!({"instance": "h", "limit": 3}));
    assert_eq!((last["first"].as_u64(), last["items"].as_array().unwrap().len()), (Some(7), 3));
    assert_eq!(last["items"][2]["text"], "m9");
    let prev = c.ok("history", json!({"instance": "h", "before": 7, "limit": 3}));
    assert_eq!((prev["first"].as_u64(), prev["total"].as_u64()), (Some(4), Some(10)));
    assert_eq!(prev["items"][0]["text"], "m4");
    assert_eq!(prev["items"][2]["text"], "m6");
    let edge = c.ok("history", json!({"instance": "h", "before": 2, "limit": 50}));
    assert_eq!((edge["first"].as_u64(), edge["items"].as_array().unwrap().len()), (Some(0), 2));
    assert_eq!(c.ok("history", json!({"instance": "h", "limit": 99999}))["items"].as_array().unwrap().len(), 10);
    // an instance without a session file yet has an empty history, an unknown one is an error
    create(&mut c, "fresh", &e.cwd, &llm, json!({}));
    assert_eq!(c.ok("history", json!({"instance": "fresh"}))["total"], 0);
    assert_eq!(c.call("history", json!({"instance": "nope"}))["ok"], false);
}

#[test]
fn history_pairs_tool_calls_and_survives_a_restart() {
    let e = Env::new("ev_hist_restart");
    let llm = FakeLlm::start_fn(|r| match n_assist(r) {
        0 => tool("bash", json!({"command": "echo hello-history"})),
        _ => text("all done"),
    });
    let mut d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "h", &e.cwd, &llm, json!({}));
    c.ok("attach", json!({"instance": "h"}));
    c.ok("prompt", json!({"instance": "h", "text": "run it"}));
    c.wait("turn", turn_end("h"));
    let live = c.ok("history", json!({"instance": "h"}));
    let kinds = |h: &Value| h["items"].as_array().unwrap().iter().map(|i| i["k"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    assert_eq!(kinds(&live), ["user", "tool", "text"], "{live}");
    let t = &live["items"][1];
    assert_eq!((t["tool"].as_str(), t["input"]["command"].as_str(), t["ok"].as_bool()), (Some("bash"), Some("echo hello-history"), Some(true)));
    assert!(t["output"].as_str().unwrap().contains("hello-history"));
    assert!(t["id"].as_str().unwrap().starts_with("call_"));
    assert_eq!(live["items"][2]["text"], "all done");
    std::thread::sleep(std::time::Duration::from_millis(400));
    drop(c);
    sigkill(&mut d);
    // a new daemon: the instance is restored but not built; the history is the same
    let d2 = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d2);
    let again = c.ok("history", json!({"instance": "h"}));
    assert_eq!(again["items"], live["items"]);
}
