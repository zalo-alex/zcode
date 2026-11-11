//! A restarted `zcode serve` recreates its instances, groups and interrupted turns from the manifests on disk.
mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};

fn text(s: &str) -> Script { Script::Text(s.into()) }
fn tool(name: &'static str, args: Value) -> Script { Script::Tools(vec![(name, args.to_string())]) }
fn all_text(r: &Value) -> String {
    r["messages"].as_array().unwrap().iter().skip(1).map(|m| format!("{}|{}", m["role"], m["content"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n")
}
fn first_user(r: &Value) -> String { r["messages"][1]["content"].as_str().unwrap_or("").to_string() }
fn turn_end(inst: &'static str) -> impl Fn(&Value) -> bool { move |e| e["type"] == "turn_end" && e["instance"] == inst }

fn sigterm(d: &mut Daemon) {
    let _ = std::process::Command::new("kill").arg(d.child.id().to_string()).status();
    let _ = d.child.wait();
}
fn sigkill(d: &mut Daemon) { let _ = d.child.kill(); let _ = d.child.wait(); }

fn history_survives(graceful: bool, name: &str) {
    let e = Env::new(name);
    let llm = FakeLlm::start_fn(|r| if all_text(r).contains("remember-me") { text("I remember") } else { text("first answer") });
    let mut d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    let made = create(&mut c, "keep", &e.cwd, &llm, json!({}));
    c.ok("attach", json!({"instance": "keep"}));
    c.ok("prompt", json!({"instance": "keep", "text": "remember-me"}));
    c.wait("first turn", turn_end("keep"));
    let seen = c.ok("status", json!({"instance": "keep"}))["instances"][0]["events"].as_u64().unwrap();
    std::thread::sleep(std::time::Duration::from_millis(500)); // the manifest writer is debounced
    drop(c);
    if graceful { sigterm(&mut d); } else { sigkill(&mut d); }

    let d2 = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d2);
    let s = c.ok("status", json!({}));
    assert_eq!(s["instances"].as_array().unwrap().len(), 1, "{s}");
    let i = &s["instances"][0];
    assert_eq!(i["instance"], "keep");
    assert!(i["events"].as_u64().unwrap() >= seen, "event counter must not go backwards: {i}");
    // the same session id comes back, and the model sees the earlier conversation
    let again = c.ok("create", json!({"instance": "keep", "reattach": true, "cwd": e.cwd, "config": client_config(&llm, "x")}));
    assert_eq!(again["session"], made["session"]);
    c.ok("attach", json!({"instance": "keep", "since": seen}));
    c.ok("prompt", json!({"instance": "keep", "text": "what did I say?"}));
    let end = c.wait("second turn", turn_end("keep"));
    assert_eq!(end["text"], "I remember");
}

#[test]
fn an_instance_and_its_history_survive_a_graceful_restart() { history_survives(true, "r_graceful"); }

#[test]
fn an_instance_and_its_history_survive_a_crash() { history_survives(false, "r_crash"); }

#[test]
fn a_closed_instance_is_not_restored() {
    let e = Env::new("r_closed");
    let llm = FakeLlm::start_fn(|_| text("ok"));
    let mut d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "gone", &e.cwd, &llm, json!({}));
    create(&mut c, "stay", &e.cwd, &llm, json!({}));
    c.ok("close", json!({"instance": "gone"}));
    sigterm(&mut d);
    let d2 = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d2);
    let s = c.ok("status", json!({}));
    let names: Vec<&str> = s["instances"].as_array().unwrap().iter().filter_map(|i| i["instance"].as_str()).collect();
    assert_eq!(names, ["stay"]);
}

#[test]
fn a_turn_interrupted_by_a_restart_resumes_by_itself() {
    let e = Env::new("r_midturn");
    let llm = FakeLlm::start_fn(|r| if all_text(r).contains("daemon restarted") { text("resumed after restart") } else { tool("bash", json!({"command": "sleep 30"})) });
    let mut d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "work", &e.cwd, &llm, json!({}));
    c.ok("attach", json!({"instance": "work"}));
    c.ok("prompt", json!({"instance": "work", "text": "long job"}));
    c.wait("tool running", |x| x["type"] == "step_start");
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(c);
    sigkill(&mut d); // crash while the tool runs: the history ends in a tool call without a result

    let d2 = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d2);
    c.ok("attach", json!({"instance": "work", "since": 0}));
    let end = c.wait("resumed turn", |x| x["type"] == "turn_end" && x["text"] == "resumed after restart");
    assert_eq!(end["code"], 0);
    assert!(c.events.iter().any(|x| x["type"] == "restored" && x["running"] == true), "{:?}", c.events);
    // the provider got a valid history: the dangling call was answered
    let last = llm.req(llm.count() - 1);
    assert!(all_text(&last).contains("interrupted"), "{}", all_text(&last));
}

#[test]
fn a_group_is_back_on_the_bus_after_a_restart() {
    let e = Env::new("r_group");
    let llm = FakeLlm::start_fn(|r| {
        let (all, me) = (all_text(r), first_user(r));
        let n = r["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").count();
        match (me.as_str(), n) {
            ("ping-b", 0) => tool("send", json!({"to": "gb", "text": "hello after restart"})),
            ("ping-b", _) => text("sent"),
            _ if all.contains("[from ga] hello after restart") => text("gb got it"),
            _ => text("idle"),
        }
    });
    let mut d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    for n in ["ga", "gb"] { create(&mut c, n, &e.cwd, &llm, json!({"group": "g"})); c.ok("build", json!({"instance": n})); }
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(c);
    sigterm(&mut d);

    let d2 = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d2);
    // both members rejoin the group without any client asking
    let t = std::time::Instant::now();
    loop {
        let s = c.ok("status", json!({"instance": "ga"}));
        if s["instances"][0]["agents"].as_array().is_some_and(|a| a.iter().filter_map(|l| l.as_str()).filter(|l| l.starts_with("ga ") || l.starts_with("gb ")).count() == 2) { break; }
        assert!(t.elapsed().as_secs() < 15, "group did not come back: {s}");
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    c.ok("attach", json!({"instance": "ga"}));
    c.ok("attach", json!({"instance": "gb"}));
    c.ok("prompt", json!({"instance": "ga", "text": "ping-b"}));
    c.wait("ga done", turn_end("ga"));
    let woke = c.wait("gb woke", |x| x["type"] == "turn_end" && x["instance"] == "gb" && x["text"] == "gb got it");
    assert_eq!(woke["code"], 0);
}

#[test]
fn workers_are_respawned_with_their_own_history() {
    let e = Env::new("r_workers");
    let llm = FakeLlm::start_fn(|r| {
        let me = first_user(r);
        if me.starts_with("[from boss]") {
            if all_text(r).contains("do the job") && all_text(r).contains("daemon restarted") { text("worker resumed") } else if r["messages"].as_array().unwrap().iter().any(|m| m["role"] == "assistant") { text("job done") } else { tool("bash", json!({"command": "sleep 30"})) }
        } else { text("lead idle") }
    });
    let mut d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "boss", &e.cwd, &llm, json!({"max_workers": 2}));
    c.ok("attach", json!({"instance": "boss"}));
    c.ok("spawn", json!({"instance": "boss", "name": "w1", "prompt": "do the job"}));
    c.wait("worker busy", |x| x["type"] == "step_start" && x["agent"] == "w1");
    std::thread::sleep(std::time::Duration::from_millis(500));
    drop(c);
    sigkill(&mut d); // the worker was mid-task

    let d2 = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d2);
    c.ok("attach", json!({"instance": "boss", "since": 0}));
    let end = c.wait("worker resumed", |x| x["type"] == "text" && x["agent"] == "w1" && x["part"]["text"] == "worker resumed");
    assert_eq!(end["agent"], "w1");
    let s = c.ok("status", json!({"instance": "boss"}));
    assert!(s["instances"][0]["agents"].as_array().unwrap().iter().any(|l| l.as_str().unwrap_or("").starts_with("w1")), "{s}");
}
