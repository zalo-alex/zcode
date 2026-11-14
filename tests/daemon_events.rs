//! Daemon protocol additions used by external UIs: `user`, `tool_start` pairing, timestamps, `status`, `history`, `cancel`, `catch_up`.
mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};

fn text(s: &str) -> Script { Script::Text(s.into()) }
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
