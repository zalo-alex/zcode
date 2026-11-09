//! `--format json` event stream: one JSON object per line on stdout, shaped like opencode's
//! (`step_start`, `text`, `tool_use`, `error`, `step_finish`) so existing parsers keep working.
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;

/// Where events go. By default stdout (`--format json`); the daemon scopes a per-instance sink around each agent task.
pub type Sink = Arc<dyn Fn(Value) + Send + Sync>;
tokio::task_local! { pub static SINK: Sink; }
pub fn current_sink() -> Option<Sink> { SINK.try_with(|s| s.clone()).ok() }

#[derive(Clone, Copy, PartialEq, Eq, Debug, clap::ValueEnum)]
pub enum Format {
    Default,
    Json,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0)
}

fn emit(session: &str, kind: &str, extra: Value) {
    // worker sessions are `<session>.<agent>`; everything else is the lead
    let agent = session.rsplit_once('.').map(|(_, a)| a).unwrap_or("lead");
    let mut v = json!({"type": kind, "timestamp": now_ms(), "sessionID": session, "agent": agent});
    if let (Some(o), Some(e)) = (v.as_object_mut(), extra.as_object()) {
        for (k, x) in e {
            o.insert(k.clone(), x.clone());
        }
    }
    if let Some(s) = current_sink() { s(v); return; }
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

/// Swarm events (`spawn`, `message`, `idle`, `port`): `agent` is set by the caller in `extra`.
pub fn custom(sid: &str, kind: &str, extra: Value) { emit(sid, kind, extra); }

/// Streaming deltas and tool starts, only emitted for daemon thin clients that render human output.
pub fn delta(sid: &str, kind: &str, t: &str) { emit(sid, kind, json!({"delta": t})); }
