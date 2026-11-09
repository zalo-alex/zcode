use crate::config::Config;
use anyhow::{Result, bail};
use regex::Regex;
use std::sync::OnceLock;
use std::time::Duration;
use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: FnCall,
}
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct FnCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCall>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    pub fn text(role: &str, s: impl Into<String>) -> Self {
        Message { role: role.into(), content: Some(s.into()), tool_calls: None, tool_call_id: None }
    }
    pub fn tool(id: &str, s: impl Into<String>) -> Self {
        Message { role: "tool".into(), content: Some(s.into()), tool_calls: None, tool_call_id: Some(id.into()) }
    }
}

/// A streamed piece of the answer: visible text or the model's reasoning ("thinking").
pub enum Delta<'a> {
    Text(&'a str),
    Reasoning(&'a str),
}

#[derive(Default, Debug)]
pub struct Reply {
    pub content: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub prompt_tokens: u64,
    pub completion_tokens: u64,
    pub finish: String,
}

pub struct Client {
    http: reqwest::Client,
    pub cfg: Config,
    idle: Duration,
}

/// Errors the agent loop reacts to differently.
#[derive(Debug)]
pub enum LlmError {
    /// prompt too large for the model
    Overflow(String),
    Http(u16, String),
    Stream(String),
}
impl std::fmt::Display for LlmError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmError::Overflow(b) => write!(f, "LLM HTTP 400: context overflow: {b}"),
            LlmError::Http(c, b) => write!(f, "LLM HTTP {c}: {b}"),
            LlmError::Stream(m) => write!(f, "stream error: {m}"),
        }
    }
}
impl std::error::Error for LlmError {}

fn overflow_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)context.{0,20}(length|window|limit)|maximum context|too many tokens|token limit|reduce the length|prompt is too long").unwrap())
}
fn upstream_err_re() -> &'static Regex {
    static R: OnceLock<Regex> = OnceLock::new();
    R.get_or_init(|| Regex::new(r"(?i)^\s*\[[^\]]*(upstream|provider|gateway)[^\]]*error[^\]]*\]\s*$").unwrap())
}

impl Client {
    pub fn new(cfg: Config) -> Self {
        let idle: f64 = std::env::var("ZCODE_STREAM_IDLE_SECS").ok().and_then(|s| s.parse().ok()).unwrap_or(120.0);
        Client {
            http: reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).build().unwrap(),
            cfg,
            idle: Duration::from_millis((idle * 1000.0) as u64),
        }
    }

    /// Stream a chat completion; `on_text` called per content delta.
    /// Retries empty replies, upstream error-as-content, connection resets, 429/5xx and stalled streams
    /// (only while nothing has been shown to the user yet).
    pub async fn chat(&self, msgs: &[Message], tools: &[Value], mut on_delta: impl FnMut(Delta)) -> Result<Reply> {
        let mut tries = 0u32;
        loop {
            let emitted = std::sync::atomic::AtomicBool::new(false);
            let res = {
                let mut wrapped = |d: Delta| { if matches!(d, Delta::Text(_)) { emitted.store(true, std::sync::atomic::Ordering::Relaxed); } on_delta(d) };
                self.chat_once(msgs, tools, &mut wrapped).await
            };
            let retry_reason = match res {
                Ok(r) if r.tool_calls.is_empty() && r.content.is_empty() => "empty reply".to_string(),
                Ok(r) if r.tool_calls.is_empty() && r.content.len() < 600 && upstream_err_re().is_match(&r.content) => r.content.trim().to_string(),
                Ok(r) => return Ok(r),
                Err(e) => {
                    let retryable = match e.downcast_ref::<LlmError>() {
                        Some(LlmError::Overflow(_)) => false,
                        Some(LlmError::Http(c, _)) => *c == 429 || *c >= 500,
                        Some(_) => true,
                        None => e.downcast_ref::<reqwest::Error>().is_some_and(|r| r.is_connect() || r.is_timeout() || r.is_request() || r.is_body()),
                    };
                    if !retryable || emitted.load(std::sync::atomic::Ordering::Relaxed) || tries >= 4 { return Err(e); }
                    e.to_string()
                }
            };
            tries += 1;
            if tries > 4 { bail!("giving up after repeated failures: {retry_reason}"); }
            let wait = Duration::from_millis(if cfg!(test) { 5 } else { (500u64 << tries).min(8000) });
            tokio::time::sleep(wait).await;
        }
    }

    async fn chat_once(&self, msgs: &[Message], tools: &[Value], on_delta: &mut impl FnMut(Delta)) -> Result<Reply> {
        let mut body = json!({
            "model": self.cfg.model,
            "messages": msgs,
            "stream": true,
            "stream_options": {"include_usage": true},
            "max_tokens": self.cfg.max_output,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools);
        }
        let mut rb = self.http.post(format!("{}/chat/completions", self.cfg.base_url)).json(&body);
        if !self.cfg.api_key.is_empty() {
            rb = rb.bearer_auth(&self.cfg.api_key);
        }
        let resp = rb.send().await?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.unwrap_or_default();
            let code = status.as_u16();
            if (code == 400 || code == 413) && overflow_re().is_match(&text) {
                return Err(LlmError::Overflow(text.chars().take(300).collect()).into());
            }
            return Err(LlmError::Http(code, text.chars().take(500).collect()).into());
        }

        let mut reply = Reply::default();
        let mut buf = String::new();
        let mut stream = resp.bytes_stream();
        let mut carry: Vec<u8> = Vec::new();
        loop {
            let chunk = match tokio::time::timeout(self.idle, stream.next()).await {
                Err(_) => return Err(LlmError::Stream(format!("no data for {}s (timeout)", self.idle.as_secs())).into()),
                Ok(None) => break,
                Ok(Some(Err(e))) => return Err(LlmError::Stream(e.to_string()).into()),
                Ok(Some(Ok(c))) => c,
            };
            carry.extend_from_slice(&chunk);
            // decode only complete utf8
            let valid = match std::str::from_utf8(&carry) {
                Ok(s) => { buf.push_str(s); carry.len() }
                Err(e) => {
                    let n = e.valid_up_to();
                    buf.push_str(std::str::from_utf8(&carry[..n]).unwrap());
                    n
                }
            };
            carry.drain(..valid);
            while let Some(i) = buf.find('\n') {
                let line = buf[..i].trim().to_string();
                buf.drain(..=i);
                let Some(data) = line.strip_prefix("data:") else { continue };
                let data = data.trim();
                if data == "[DONE]" { return Ok(finish(reply)); }
                let Ok(v) = serde_json::from_str::<Value>(data) else { continue };
                apply_chunk(&mut reply, &v, on_delta);
            }
        }
        Ok(finish(reply))
    }
}

fn finish(mut r: Reply) -> Reply {
    r.tool_calls.retain(|t| !t.function.name.is_empty());
    for t in &mut r.tool_calls {
        t.kind = "function".into();
        if t.function.arguments.trim().is_empty() { t.function.arguments = "{}".into(); }
    }
    r
}

/// Apply one SSE JSON chunk to reply (assembles tool_call deltas by index).
pub fn apply_chunk(reply: &mut Reply, v: &Value, on_delta: &mut impl FnMut(Delta)) {
    if let Some(u) = v.get("usage").filter(|u| !u.is_null()) {
        reply.prompt_tokens = u["prompt_tokens"].as_u64().unwrap_or(0);
        reply.completion_tokens = u["completion_tokens"].as_u64().unwrap_or(0);
    }
    if let Some(f) = v["choices"].get(0).and_then(|c| c["finish_reason"].as_str()) {
        reply.finish = f.to_string();
    }
    let Some(delta) = v["choices"].get(0).map(|c| &c["delta"]) else { return };
    if let Some(t) = delta["reasoning_content"].as_str().or(delta["reasoning"].as_str()).filter(|t| !t.is_empty()) {
        reply.reasoning.push_str(t);
        on_delta(Delta::Reasoning(t));
    }
    if let Some(t) = delta["content"].as_str().filter(|t| !t.is_empty()) {
        reply.content.push_str(t);
        on_delta(Delta::Text(t));
    }
    if let Some(tcs) = delta["tool_calls"].as_array() {
        for tc in tcs {
            let idx = tc["index"].as_u64().unwrap_or(0) as usize;
            while reply.tool_calls.len() <= idx {
                reply.tool_calls.push(ToolCall { id: String::new(), kind: "function".into(), function: FnCall::default() });
            }
            let slot = &mut reply.tool_calls[idx];
            if let Some(id) = tc["id"].as_str().filter(|i| !i.is_empty()) { slot.id = id.into(); }
            if let Some(n) = tc["function"]["name"].as_str() { slot.function.name.push_str(n); }
            if let Some(a) = tc["function"]["arguments"].as_str() { slot.function.arguments.push_str(a); }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assembles_tool_deltas() {
        let mut r = Reply::default();
        let mut sink = |_: Delta| {};
        for c in [
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"id":"c1","function":{"name":"read","arguments":"{\"pa"}}]}}]}),
            json!({"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"th\":\"a\"}"}}]}}]}),
        ] { apply_chunk(&mut r, &c, &mut sink); }
        let r = finish(r);
        assert_eq!(r.tool_calls[0].function.name, "read");
        assert_eq!(r.tool_calls[0].function.arguments, "{\"path\":\"a\"}");
    }

    #[test]
    fn captures_reasoning_separately_from_text() {
        let mut r = Reply::default();
        let mut seen: Vec<String> = vec![];
        let mut cb = |d: Delta| seen.push(match d { Delta::Text(t) => format!("T:{t}"), Delta::Reasoning(t) => format!("R:{t}") });
        for c in [
            json!({"choices":[{"delta":{"reasoning_content":"hmm "}}]}),
            json!({"choices":[{"delta":{"reasoning":"ok"}}]}),
            json!({"choices":[{"delta":{"content":"answer"}}]}),
        ] { apply_chunk(&mut r, &c, &mut cb); }
        assert_eq!((r.reasoning.as_str(), r.content.as_str()), ("hmm ok", "answer"));
        assert_eq!(seen, ["R:hmm ", "R:ok", "T:answer"]);
    }
}
