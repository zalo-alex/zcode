//! Daemon protocol additions used by external UIs: `user`, `tool_start` pairing, timestamps, `status`, `history`, `cancel`, `catch_up`.
mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};

fn text(s: &str) -> Script { Script::Text(s.into()) }
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
