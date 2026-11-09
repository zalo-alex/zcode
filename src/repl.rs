use crate::agent::{Agent, Stop};
use crate::{context, session};
use crate::style::Paint;
use rustyline::{DefaultEditor, error::ReadlineError};

const HELP: &str = "/help  /clear  /compact  /thinking  /sessions  /status  /todo  /cost  /agents  /model  /exit   (Ctrl-C interrupts a turn)";

fn last_assistant_tail(agent: &Agent) -> Option<String> {
    let m = agent.msgs.iter().rev().find(|m| m.role == "assistant" && m.content.as_ref().is_some_and(|c| !c.trim().is_empty()))?;
    let c = m.content.as_ref()?.trim();
    let start = c.len().saturating_sub(600);
    Some(c[c.ceil_char_boundary(start)..].to_string())
}

pub async fn run(mut agent: Agent, resumed: bool) -> anyhow::Result<()> {
    let mut rl = DefaultEditor::new()?;
    println!("{} {} · {} · {}", "zcode".bold().cyan(), env!("CARGO_PKG_VERSION"), agent.client.cfg.model, agent.ctx.cwd().display());
    println!("{}", format!("session {}{}", agent.sid(), if agent.perms.yes { " · auto-approve" } else { "" }).dimmed());
    println!("{}", HELP.dimmed());
    if resumed {
        let n = agent.msgs.len().saturating_sub(1);
        println!("{}", format!("resumed session ({n} messages). last reply:").dimmed());
        if let Some(t) = last_assistant_tail(&agent) { println!("{t}"); }
    }
    loop {
        let line = match rl.readline("› ") {
            Ok(l) => l,
            Err(ReadlineError::Interrupted) => continue,
            Err(_) => break,
        };
        let line = line.trim();
        if line.is_empty() { continue; }
        rl.add_history_entry(line).ok();
        if let Some(cmd) = line.strip_prefix('/') {
            match cmd.split_whitespace().next().unwrap_or("") {
                "exit" | "quit" | "q" => break,
                "help" => println!("{HELP}"),
                "thinking" => { agent.thinking = !agent.thinking; println!("thinking display {}", if agent.thinking { "on" } else { "off" }); }
                "clear" => { agent.reset(); println!("cleared"); }
                "compact" => match agent.shrink(true).await {
                    Ok(_) => println!("compacted → ~{} tokens", context::estimate(&agent.msgs)),
                    Err(e) => println!("error: {e}"),
                },
                "sessions" => {
                    for i in session::list().iter().take(10) { println!("{}  {} msgs  {}", i.id, i.messages, i.cwd); }
                    println!("resume with: zcode --session <id>   or   zcode --continue");
                }
                "status" => println!("session {} · {} msgs · ~{}/{} tokens · {} · thinking {} · cwd {}", agent.sid(), agent.msgs.len() - 1,
                    context::estimate(&agent.msgs), agent.client.cfg.context, agent.client.cfg.model, if agent.thinking { "on" } else { "off" }, agent.ctx.cwd().display()),
                "todo" => {
                    for (s, c) in agent.ctx.todos.lock().unwrap().iter() { println!("[{}] {c}", match s.as_str() { "completed" => "x", "in_progress" => "~", _ => " " }); }
                }
                "cost" => {
                    println!("tokens in {} / out {} · ctx ~{}/{}", agent.tokens_in, agent.tokens_out, context::estimate(&agent.msgs), agent.client.cfg.context);
                    if let Some(sw) = &agent.swarm {
                        let t = sw.totals();
                        for (n, i, o) in &t { println!("  {n}: in {i} / out {o}"); }
                        println!("  swarm total: in {} / out {}", t.iter().map(|x| x.1).sum::<u64>(), t.iter().map(|x| x.2).sum::<u64>());
                    }
                }
                "agents" => match &agent.swarm {
                    Some(sw) => for l in sw.list() { println!("{l}"); },
                    None => println!("swarm is off (start with --max-workers N)"),
                },
                "model" => println!("{}", agent.client.cfg.model),
                _ => println!("unknown command; {HELP}"),
            }
            continue;
        }
        match agent.turn(line).await {
            Ok(o) => if let Stop::Limit(_) = o.stop { /* message already printed */ },
            // errors stay on screen verbatim (anything watching the terminal can see 429/rate limit/timeout/...); the REPL keeps running so "continue" resumes
            Err(e) => eprintln!("{} {e}", "error:".red()),
        }
    }
    if let Some(sw) = &agent.swarm { sw.shutdown().await; }
    Ok(())
}
