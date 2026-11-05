//! `--format json` event stream: one JSON object per line on stdout, shaped like opencode's
//! (`step_start`, `text`, `tool_use`, `error`, `step_finish`) so existing parsers keep working.
use serde_json::{Value, json};
use std::io::Write;

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Format {
    Default,
    Json,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn emit(session: &str, kind: &str, extra: Value) {
    let mut v = json!({"type": kind, "timestamp": now_ms(), "sessionID": session});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{v}");
    let _ = out.flush();
}

pub fn step_start(sid: &str) {
    emit(sid, "step_start", json!({"part": {"type": "step-start"}}));
}
pub fn reasoning(sid: &str, t: &str) {
    emit(sid, "reasoning", json!({"part": {"type": "reasoning", "text": t}}));
}
pub fn text(sid: &str, t: &str) {
    emit(sid, "text", json!({"part": {"type": "text", "text": t}}));
}
pub fn tool_use(sid: &str, call_id: &str, name: &str, input: &Value, output: &str) {
    let status = if output.starts_with("Error:") { "error" } else { "completed" };
    emit(sid, "tool_use", json!({"part": {"type": "tool", "tool": name, "callID": call_id,
        "state": {"status": status, "input": input, "output": output}}}));
}
pub fn error(sid: &str, kind: &str, message: &str) {
    emit(sid, "error", json!({"error": {"type": kind, "message": message}}));
}
pub fn step_finish(sid: &str, reason: &str, tin: u64, tout: u64) {
    emit(sid, "step_finish", json!({"part": {"type": "step-finish", "reason": reason, "tokens": {"input": tin, "output": tout}}}));
}
