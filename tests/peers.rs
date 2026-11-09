//! Two interactive `zcode --group` agents in a group, through one daemon: they message each other without a lead.
mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

fn text(s: &str) -> Script { Script::Text(s.into()) }
fn tool(name: &'static str, args: Value) -> Script { Script::Tools(vec![(name, args.to_string())]) }
fn n_assist(r: &Value) -> usize { r["messages"].as_array().unwrap().iter().filter(|m| m["role"] == "assistant").count() }
fn all_text(r: &Value) -> String {
    r["messages"].as_array().unwrap().iter().skip(1).map(|m| m["content"].as_str().unwrap_or("").to_string()).collect::<Vec<_>>().join("\n")
}
/// the agent a request belongs to: its system prompt says "You are agent `NAME` in the group"
fn who(r: &Value) -> String {
    let sys = r["messages"][0]["content"].as_str().unwrap_or("");
    sys.split_once("You are agent `").and_then(|(_, rest)| rest.split('`').next()).unwrap_or("?").to_string()
}

struct Term {
    child: Child,
    stdin: Option<ChildStdin>,
    lines: Arc<Mutex<Vec<String>>>,
}
impl Term {
    fn say(&mut self, l: &str) { let s = self.stdin.as_mut().unwrap(); writeln!(s, "{l}").unwrap(); s.flush().unwrap(); }
    fn screen(&self) -> String { self.lines.lock().unwrap().join("\n") }
    fn wait_for(&self, what: &str) {
        let t = Instant::now();
        while !self.screen().contains(what) {
            assert!(t.elapsed() < Duration::from_secs(20), "timed out waiting for {what:?}; screen:\n{}", self.screen());
            std::thread::sleep(Duration::from_millis(30));
        }
    }
}
impl Drop for Term { fn drop(&mut self) { let _ = self.child.kill(); let _ = self.child.wait(); } }

fn join(e: &Env, llm: &FakeLlm, group: &str, name: &str, extra: &[&str]) -> Term {
    let mut args = vec!["--group", group, "--agent", name, "--auto", "--sandbox", "none"];
    args.extend_from_slice(extra);
    let mut c = e.cmd(llm, &args);
    c.env("XDG_RUNTIME_DIR", e.dir.join("runtime")).env("XDG_STATE_HOME", e.home.join("state")).env("ZCODE_NO_DAEMON", "1")
        .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = c.spawn().unwrap();
    let lines: Arc<Mutex<Vec<String>>> = Default::default();
    for r in [Box::new(child.stdout.take().unwrap()) as Box<dyn std::io::Read + Send>, Box::new(child.stderr.take().unwrap())] {
        let l = lines.clone();
        std::thread::spawn(move || for x in BufReader::new(r).lines().map_while(Result::ok) { l.lock().unwrap().push(x); });
    }
    let stdin = child.stdin.take();
    Term { child, stdin, lines }
}

#[test]
fn two_interactive_agents_message_each_other_without_a_lead() {
    let e = Env::new("p_chat");
    let llm = FakeLlm::start_fn(|r| {
        let (me, all, n) = (who(r), all_text(r), n_assist(r));
        match me.as_str() {
            // alice's user asks her to tell bob; bob's reply wakes her again
            "alice" if all.contains("[from bob] got it") => text("bob confirmed the plan"),
            "alice" if n == 0 => tool("send", json!({"to": "bob", "text": "the plan is ready: see PLAN.md"})),
            "alice" => text("I told bob."),
            // bob is woken by alice's message: nobody typed anything in his terminal
            "bob" if all.contains("[from alice] the plan is ready") && n == 0 => tool("send", json!({"to": "alice", "text": "got it, starting now"})),
            "bob" => text("I answered alice."),
            _ => text("?"),
        }
    });
    let d = start(&e, json!({}), &[]);
    let mut alice = join(&e, &llm, "duo", "alice", &[]);
    alice.wait_for("joined group `duo` as `alice`");
    let mut bob = join(&e, &llm, "duo", "bob", &[]);
    bob.wait_for("in the group: alice, bob");

    alice.say("tell bob the plan is ready");
    alice.wait_for("bob confirmed the plan");
    let (a, b) = (alice.screen(), bob.screen());
    assert!(a.contains("▸ send bob: the plan is ready"), "alice shows what she sent:\n{a}");
    assert!(b.contains("‹ alice") && b.contains("the plan is ready: see PLAN.md"), "bob sees the incoming message with no input:\n{b}");
    assert!(b.contains("I answered alice."), "bob's agent worked on it by itself:\n{b}");
    assert!(a.contains("‹ bob") && a.contains("got it, starting now"), "alice sees bob's reply arrive:\n{a}");

    // both are listed by the daemon; leaving removes just that agent
    let mut c = Client::connect(&d);
    let s = c.ok("status", json!({}));
    let mut ids: Vec<String> = s["instances"].as_array().unwrap().iter().map(|i| i["instance"].as_str().unwrap().to_string()).collect();
    ids.sort();
    assert_eq!(ids, ["alice", "bob"]);
    bob.say("/agents");
    bob.wait_for("alice");
    alice.say("/exit");
    let t = Instant::now();
    loop {
        let s = c.ok("status", json!({}));
        if s["instances"].as_array().unwrap().len() == 1 { break; }
        assert!(t.elapsed() < Duration::from_secs(10), "alice did not leave: {s}");
        std::thread::sleep(Duration::from_millis(50));
    }
    // a terminal that dies takes its agent with it: nobody is left to read what it would answer
    drop(bob);
    let t = Instant::now();
    loop {
        let s = c.ok("status", json!({}));
        if s["instances"].as_array().unwrap().is_empty() { break; }
        assert!(t.elapsed() < Duration::from_secs(10), "bob lingered after his terminal died: {s}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn what_you_type_while_your_agent_is_busy_is_queued_not_lost() {
    let e = Env::new("p_queue");
    // the first request keeps the agent busy for a second (a slow command); the second is answered at once
    let llm = FakeLlm::start_fn(|r| {
        let last = r["messages"].as_array().unwrap().last().unwrap().clone();
        let (role, content) = (last["role"].as_str().unwrap_or(""), last["content"].as_str().unwrap_or(""));
        match (role, content) {
            ("user", "first") => tool("bash", json!({"command": "sleep 1"})),
            ("tool", _) => text("first-done"),
            _ => text("second-done"),
        }
    });
    let _d = start(&e, json!({}), &[]);
    let mut t = join(&e, &llm, "solo", "carol", &[]);
    t.wait_for("joined group `solo` as `carol`");
    t.say("first");
    t.say("second");
    t.wait_for("(your agent is busy; sent when it is done)");
    t.wait_for("second-done");
}

#[test]
fn joining_needs_a_daemon_auto_and_a_free_name() {
    let e = Env::new("p_err");
    let llm = FakeLlm::start_fn(|_| text("ok"));
    let run = |args: &[&str]| {
        let mut c = e.cmd(&llm, args);
        c.env("XDG_RUNTIME_DIR", e.dir.join("runtime")).env("XDG_STATE_HOME", e.home.join("state"));
        std::fs::create_dir_all(e.dir.join("runtime")).unwrap();
        c.output().unwrap()
    };
    let o = run(&["--group", "g", "--auto"]);
    assert_eq!(code(&o), 1);
    assert!(err(&o).contains("no zcode daemon is running") && err(&o).contains("zcode serve"), "{}", err(&o));
    let o = run(&["--group", "g"]);
    assert!(err(&o).contains("add --auto"), "{}", err(&o));
    let o = run(&["--group", "g", "--auto", "run", "hello"]);
    assert!(err(&o).contains("interactive"), "{}", err(&o));
    let o = run(&["--agent", "x"]);
    assert_ne!(code(&o), 0, "--agent needs --group");

    let _d = start(&e, json!({}), &[]);
    let mut first = join(&e, &llm, "g", "dup", &[]);
    first.wait_for("joined group");
    let mut second = join(&e, &llm, "g", "dup", &[]);
    let t = Instant::now();
    while second.child.try_wait().unwrap().is_none() {
        assert!(t.elapsed() < Duration::from_secs(10), "the second `dup` should be refused");
        std::thread::sleep(Duration::from_millis(30));
    }
    assert!(second.screen().contains("already exists") && second.screen().contains("--agent"), "{}", second.screen());
    first.say("/exit");
}
