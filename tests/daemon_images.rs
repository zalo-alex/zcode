//! Images through the daemon: `prompt` with `images`, and `zcode run --image` as a thin client.
mod common;
mod dm;
use common::*;
use dm::*;
use serde_json::{Value, json};

/// A valid 1x1 PNG.
const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn turn_end(inst: &'static str) -> impl Fn(&Value) -> bool { move |e| e["type"] == "turn_end" && e["instance"] == inst }
fn user_msg(r: &Value) -> Value { r["messages"].as_array().unwrap().iter().rev().find(|m| m["role"] == "user").unwrap().clone() }

fn vision_cfg(llm: &FakeLlm, vision: bool) -> Value {
    let mut c = client_config(llm, "sk-client-secret");
    c["vision"] = json!(vision);
    c
}

#[test]
fn prompt_with_images_reaches_a_vision_model_as_parts() {
    let e = Env::new("di_parts");
    let llm = FakeLlm::start_fn(|_| Script::Text("seen".into()));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "v", &e.cwd, &llm, json!({"config": vision_cfg(&llm, true)}));
    c.ok("attach", json!({"instance": "v"}));
    c.ok("prompt", json!({"instance": "v", "text": "what is this?", "images": [{"data": PNG_B64}]}));
    c.wait("turn end", turn_end("v"));
    let m = user_msg(&llm.req(0));
    let parts = m["content"].as_array().unwrap_or_else(|| panic!("not parts: {m}"));
    assert_eq!((parts[0]["type"].as_str(), parts[0]["text"].as_str()), (Some("text"), Some("what is this?")));
    assert_eq!(parts[1]["type"], "image_url");
    // the attachment is for that message only
    c.ok("prompt", json!({"instance": "v", "text": "and now?"}));
    c.wait("turn end", turn_end("v"));
    assert_eq!(user_msg(&llm.req(1))["content"], "and now?");
    // an image alone is a valid prompt
    c.ok("prompt", json!({"instance": "v", "text": "", "images": [{"data": PNG_B64}]}));
    c.wait("turn end", turn_end("v"));
    let m = user_msg(&llm.req(2));
    assert_eq!(m["content"].as_array().unwrap().len(), 1);
}

#[test]
fn a_text_only_model_gets_a_note_and_bad_images_are_refused() {
    let e = Env::new("di_blind");
    let llm = FakeLlm::start_fn(|_| Script::Text("ok".into()));
    let d = start(&e, json!({}), &[]);
    let mut c = Client::connect(&d);
    create(&mut c, "b", &e.cwd, &llm, json!({"config": vision_cfg(&llm, false)}));
    c.ok("attach", json!({"instance": "b"}));
    c.ok("prompt", json!({"instance": "b", "text": "look", "images": [{"data": PNG_B64}]}));
    c.wait("turn end", turn_end("b"));
    let m = user_msg(&llm.req(0));
    assert!(m["content"].as_str().unwrap().contains("this model cannot see it"), "{m}");
    assert!(!llm.req(0).to_string().contains("image_url"));
    // refusals leave the instance usable and send nothing
    for (imgs, want) in [(json!([{"data": "!!!"}]), "invalid base64"), (json!([{"data": "aGVsbG8="}]), "not a supported image"), (json!([{}]), "needs a `data`"), (json!("x"), "must be an array")] {
        let r = c.call("prompt", json!({"instance": "b", "text": "x", "images": imgs}));
        assert_eq!(r["ok"], false, "{r}");
        assert!(r["err"].as_str().unwrap().contains(want), "{r}");
    }
    let r = c.call("prompt", json!({"instance": "b", "text": " "}));
    assert!(r["err"].as_str().unwrap().contains("empty prompt"), "{r}");
    assert_eq!(llm.count(), 1);
    c.ok("prompt", json!({"instance": "b", "text": "still fine"}));
    c.wait("turn end", turn_end("b"));
    assert_eq!(user_msg(&llm.req(1))["content"], "still fine");
}

#[test]
fn zcode_run_image_through_the_daemon_keeps_the_image() {
    let e = Env::new("di_thin");
    std::fs::write(e.cwd.join("a.png"), zcode_tools::img::b64_decode(PNG_B64).unwrap()).unwrap();
    let llm = FakeLlm::start_fn(|_| Script::Text("fine".into()));
    let d = start(&e, json!({}), &[]);
    let o = e.cmd_cfg(&llm, json!({"vision": true}), &["run", "--auto", "--image", "a.png", "describe"])
        .env("XDG_RUNTIME_DIR", d.sock.parent().unwrap().parent().unwrap()).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    assert!(d.home.join("data/zcode/sessions").read_dir().unwrap().count() >= 1, "the daemon ran it");
    let m = user_msg(&llm.req(0));
    let parts = m["content"].as_array().unwrap_or_else(|| panic!("image was dropped on the way: {m}"));
    assert_eq!(parts[1]["type"], "image_url");
}
