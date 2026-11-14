mod common;
use common::*;
use serde_json::json;
use zcode_tools::img::b64_encode;

/// A valid 1x1 PNG.
const PNG_B64: &str = "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==";

fn png() -> Vec<u8> { zcode_tools::img::b64_decode(PNG_B64).unwrap() }

#[test]
fn read_on_a_png_sends_an_image_part_to_a_vision_model() {
    let e = Env::new("img_read");
    std::fs::write(e.cwd.join("shot.png"), png()).unwrap();
    let llm = FakeLlm::start(vec![Script::Tools(vec![("read", json!({"path": "shot.png"}).to_string())]), Script::Text("seen".into())]);
    let o = e.cmd_cfg(&llm, json!({"vision": true}), &["run", "--yes", "look"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let r = llm.req(1);
    let tool = r["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
    let parts = tool["content"].as_array().unwrap_or_else(|| panic!("content is not an array: {tool}"));
    assert_eq!(parts.len(), 2);
    assert_eq!(parts[0]["type"], "text");
    assert!(parts[0]["text"].as_str().unwrap().ends_with("shot.png (1x1)"), "{}", parts[0]);
    assert_eq!(parts[1]["type"], "image_url");
    assert!(parts[1]["image_url"]["url"].as_str().unwrap().starts_with("data:image/jpeg;base64,"));
    assert!(!r.to_string().contains("binary file"));
}

#[test]
fn a_text_only_model_gets_one_string_and_never_parts() {
    let e = Env::new("img_blind");
    std::fs::write(e.cwd.join("shot.png"), png()).unwrap();
    let llm = FakeLlm::start(vec![Script::Tools(vec![("read", json!({"path": "shot.png"}).to_string())]), Script::Text("ok".into())]);
    let o = e.run(&llm, &["run", "--yes", "look"]); // model "m": no vision
    assert_eq!(code(&o), 0, "{}", err(&o));
    let r = llm.req(1);
    let tool = r["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
    let s = tool["content"].as_str().unwrap();
    assert!(s.contains("shot.png (1x1)") && s.contains("[image attached; this model cannot see it]"), "{s}");
    assert!(!r.to_string().contains("image_url") && !r.to_string().contains("base64"));
}

#[test]
fn bash_printing_base64_is_converted_not_truncated() {
    let e = Env::new("img_bash");
    // a big PNG-looking payload: the PNG header makes it an image, the rest is padding well over the 30 KB tool cap
    let mut big = png();
    big.extend(std::iter::repeat_n(0u8, 200_000));
    std::fs::write(e.cwd.join("big.b64"), b64_encode(&big)).unwrap();
    std::fs::write(e.cwd.join("small.b64"), PNG_B64).unwrap();
    let llm = FakeLlm::start(vec![Script::Tools(vec![("bash", json!({"command": "cat small.b64"}).to_string())]), Script::Text("ok".into())]);
    let o = e.cmd_cfg(&llm, json!({"vision": true}), &["run", "--yes", "x"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let r = llm.req(1);
    let tool = r["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
    let parts = tool["content"].as_array().unwrap();
    assert!(parts[0]["text"].as_str().unwrap().starts_with("Image from command output"), "{}", parts[0]);
    assert_eq!(parts[1]["type"], "image_url");
    // the raw base64 text never reaches the model
    assert!(!r.to_string().contains(PNG_B64));
    // large output is not mangled by the 30 KB cap (the PNG is invalid after the header, so it becomes a note, not a truncated blob)
    let llm = FakeLlm::start(vec![Script::Tools(vec![("bash", json!({"command": "cat big.b64"}).to_string())]), Script::Text("ok".into())]);
    let o = e.cmd_cfg(&llm, json!({"vision": true}), &["run", "--yes", "x"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let s = llm.req(1).to_string();
    assert!(!s.contains("bytes truncated") && !s.contains("full output saved"), "{s}");
}

#[test]
fn image_flag_attaches_to_the_first_message() {
    let e = Env::new("img_flag");
    std::fs::write(e.cwd.join("a.png"), png()).unwrap();
    let llm = FakeLlm::start(vec![Script::Text("ok".into())]);
    let o = e.cmd_cfg(&llm, json!({"vision": true}), &["run", "--yes", "--image", "a.png", "what is this?"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let r = llm.req(0);
    let user = r["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap();
    let parts = user["content"].as_array().unwrap();
    assert_eq!((parts[0]["type"].as_str(), parts[0]["text"].as_str()), (Some("text"), Some("what is this?")));
    assert_eq!(parts[1]["type"], "image_url");
    // a text-only model: plain string with the note
    let llm = FakeLlm::start(vec![Script::Text("ok".into())]);
    let o = e.run(&llm, &["run", "--yes", "--image", "a.png", "what is this?"]);
    assert_eq!(code(&o), 0, "{}", err(&o));
    let r = llm.req(0);
    let user = r["messages"].as_array().unwrap().iter().find(|m| m["role"] == "user").unwrap();
    assert!(user["content"].as_str().unwrap().contains("this model cannot see it"));
    // not an image: clear error, no request
    std::fs::write(e.cwd.join("t.txt"), "hi").unwrap();
    let llm = FakeLlm::start(vec![]);
    let o = e.run(&llm, &["run", "--yes", "--image", "t.txt", "x"]);
    assert_ne!(code(&o), 0);
    assert!(err(&o).contains("not a supported image"), "{}", err(&o));
    assert_eq!(llm.count(), 0);
}

#[test]
fn text_only_conversations_are_unchanged() {
    let e = Env::new("img_text");
    std::fs::write(e.cwd.join("f.txt"), "hello\n").unwrap();
    let llm = FakeLlm::start(vec![Script::Tools(vec![("read", json!({"path": "f.txt"}).to_string())]), Script::Text("ok".into())]);
    let o = e.cmd_cfg(&llm, json!({"vision": true}), &["run", "--yes", "x"]).output().unwrap();
    assert_eq!(code(&o), 0, "{}", err(&o));
    let r = llm.req(1);
    let ms = r["messages"].as_array().unwrap();
    assert!(ms.iter().all(|m| m["content"].is_string() || m["content"].is_null()), "{r}");
    assert!(ms.iter().find(|m| m["role"] == "tool").unwrap()["content"].as_str().unwrap().contains("hello"));
}
