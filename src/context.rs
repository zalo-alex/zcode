use crate::llm::{Client, Message};
use anyhow::Result;

pub fn estimate(msgs: &[Message]) -> usize {
    msgs.iter().map(|m| {
        m.content.as_ref().map_or(0, |c| c.len())
            + m.tool_calls.as_ref().map_or(0, |t| t.iter().map(|c| c.function.arguments.len() + c.function.name.len()).sum())
    }).sum::<usize>() / 4
}

/// Cheap first step: shrink big tool outputs older than the last few messages. Returns bytes saved.
pub fn prune(msgs: &mut [Message]) -> usize {
    let keep_from = msgs.len().saturating_sub(6);
    let mut saved = 0;
    for m in msgs.iter_mut().take(keep_from) {
        if m.role == "tool"
            && let Some(c) = &mut m.content
            && c.len() > 2000
        {
            let head = &c[..c.floor_char_boundary(1000)];
            let new = format!("{head}\n[... old tool output pruned: {} bytes ...]", c.len() - 1000);
            saved += c.len() - new.len();
            *c = new;
        }
    }
    saved
}

/// Prune, then (if still needed) summarize old messages with the LLM. Keeps system + summary + recent tail.
/// Returns true when the conversation changed.
pub async fn shrink(client: &Client, msgs: &mut Vec<Message>, force: bool, limit_tokens: usize, current_tokens: usize) -> Result<bool> {
    let pruned = prune(msgs) > 0;
    if !force && current_tokens.saturating_sub(estimate(msgs)) < limit_tokens && estimate(msgs) < limit_tokens * 6 / 10 { return Ok(pruned); }
    let before = msgs.len();
    compact(client, msgs).await?;
    Ok(pruned || msgs.len() != before)
}

/// Summarize old messages, keep system + summary + recent tail.
pub async fn compact(client: &Client, msgs: &mut Vec<Message>) -> Result<()> {
    if msgs.len() < 6 { return Ok(()); }
    // keep last ~4 messages, but never split a tool result from its assistant call
    let mut cut = msgs.len() - 4;
    while cut > 1 && msgs[cut].role == "tool" { cut -= 1; }
    if cut <= 1 { return Ok(()); }
    let mut transcript = String::new();
    for m in &msgs[1..cut] {
        let body = m.content.clone().unwrap_or_default();
        let body = if body.len() > 1500 { format!("{}…", &body[..body.floor_char_boundary(1500)]) } else { body };
        let calls = m.tool_calls.as_ref().map(|t| t.iter().map(|c| format!("{}({})", c.function.name, c.function.arguments)).collect::<Vec<_>>().join("; ")).unwrap_or_default();
        transcript.push_str(&format!("[{}] {} {}\n", m.role, body, calls));
    }
    let req = vec![
        Message::text("system", "Summarize this coding-agent conversation for continuation. Keep: user goal, decisions, files touched, key findings, current state, pending work. Be dense."),
        Message::text("user", transcript),
    ];
    let r = client.chat(&req, &[], |_| {}).await?;
    let tail = msgs.split_off(cut);
    msgs.truncate(1);
    msgs.push(Message::text("user", format!("[Summary of earlier conversation]\n{}", r.content)));
    msgs.push(Message::text("assistant", "Understood. Continuing."));
    msgs.extend(tail);
    Ok(())
}
