mod agent;
mod config;
mod context;
mod events;
mod guard;
mod llm;
mod permission;
mod prompt;
mod repl;
mod session;
mod style;
mod tools;

use agent::{Agent, AgentOpts, Limits, Stop};
use clap::{Args, Parser, Subcommand};
use events::Format;
use guard::GuardMode;
use std::io::{IsTerminal, Read};
use std::sync::Arc;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "zcode", version = concat!(env!("CARGO_PKG_VERSION"), env!("ZCODE_BUILD")), about = "fast lightweight coding agent",
    args_conflicts_with_subcommands = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    #[command(flatten)]
    common: Common,
    /// Prompt (same as `zcode run`)
    #[arg(short = 'p', long)]
    prompt: Option<String>,
    /// Positional prompt (same as -p)
    rest: Vec<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run one prompt non-interactively and exit
    Run {
        #[command(flatten)]
        common: Common,
        /// Message (read from stdin when empty and stdin is piped)
        message: Vec<String>,
    },
    /// List saved sessions
    Sessions,
    /// Update zcode to the latest GitHub release
    Upgrade,
}

#[derive(Args, Clone)]
struct Common {
    /// provider/model override
    #[arg(short, long, global = true)]
    model: Option<String>,
    /// Auto-approve all tool calls (no prompts); also lifts the outside-cwd restriction
    #[arg(short = 'y', long, visible_alias = "yes", global = true)]
    auto: bool,
    /// Working directory
    #[arg(short = 'C', long, global = true)]
    cwd: Option<std::path::PathBuf>,
    /// Continue the latest session of this directory
    #[arg(short = 'c', long = "continue", visible_short_alias = 'r', visible_alias = "resume", global = true)]
    cont: bool,
    /// Use (or create) this session id
    #[arg(short, long, global = true)]
    session: Option<String>,
    /// Output format: default (human) or json (one event per line)
    #[arg(long, value_enum, default_value = "default", global = true)]
    format: Format,
    /// Allow write/edit outside the working directory
    #[arg(long, global = true)]
    allow_outside: bool,
    /// What to do after untrusted content tried to instruct the agent (default: ask on a terminal, deny with --auto or no terminal)
    #[arg(long, value_enum, global = true)]
    guard: Option<GuardMode>,
    /// Wall-clock limit per turn in seconds, 0 = none (default: 1200 for `run`, none in the REPL)
    #[arg(long, global = true)]
    max_time: Option<u64>,
    /// Show the model's reasoning (default: on in the REPL, off for `run`)
    #[arg(long, global = true)]
    thinking: bool,
    /// Hide the model's reasoning
    #[arg(long, global = true, conflicts_with = "thinking")]
    no_thinking: bool,
    /// Max model steps per turn
    #[arg(long, default_value_t = 60, global = true)]
    max_steps: usize,
    /// Max cumulative prompt tokens per turn
    #[arg(long, default_value_t = 400_000, global = true)]
    max_tokens: u64,
}

fn install_signal_handlers() {
    // sessions are append-only, so nothing to flush: just exit quietly like a normal terminated process
    #[cfg(unix)]
    tokio::spawn(async {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut term), Ok(mut hup)) = (signal(SignalKind::terminate()), signal(SignalKind::hangup())) else { return };
        tokio::select! { _ = term.recv() => {}, _ = hup.recv() => {} }
        std::process::exit(143);
    });
}

fn read_stdin() -> String {
    let mut s = String::new();
    let _ = std::io::stdin().read_to_string(&mut s);
    s.trim().to_string()
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();
    let json = match &cli.cmd { Some(Cmd::Run { common, .. }) => common.format, _ => cli.common.format } == Format::Json;
    let code = match real_main(cli).await {
        Ok(c) => c,
        Err(e) => {
            // setup failures (bad model, bad config, bad session id) must be visible to JSON consumers too
            if json { events::error("", "config", &e.to_string()); } else { eprintln!("error: {e}"); }
            1
        }
    };
    std::process::exit(code);
}

async fn real_main(cli: Cli) -> anyhow::Result<i32> {
    style::init();
    install_signal_handlers();
    let (common, prompt, is_run) = match cli.cmd {
        Some(Cmd::Sessions) => return list_sessions(),
        Some(Cmd::Upgrade) => return upgrade(),
        Some(Cmd::Run { common, message }) => {
            let mut p = message.join(" ");
            if p.trim().is_empty() && !std::io::stdin().is_terminal() { p = read_stdin(); }
            if p.trim().is_empty() { anyhow::bail!("`zcode run` needs a message (argument or stdin)"); }
            (common, Some(p), true)
        }
        None => {
            let p = cli.prompt.or_else(|| if cli.rest.is_empty() { None } else { Some(cli.rest.join(" ")) });
            let one_shot = p.is_some();
            (cli.common, p, one_shot)
        }
    };
    if let Some(d) = &common.cwd { std::env::set_current_dir(d)?; }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let settings = config::Settings::load(common.model.as_deref())?;
    let auto = common.auto || settings.auto;
    let max_time = match common.max_time { Some(0) => None, Some(s) => Some(Duration::from_secs(s)), None => is_run.then_some(Duration::from_secs(1200)) };
    let guard = common.guard.unwrap_or(if auto || !std::io::stdin().is_terminal() { GuardMode::Deny } else { GuardMode::Ask });
    let thinking = if common.no_thinking { false } else if common.thinking { true } else { settings.thinking.unwrap_or(!is_run) };
    let model_name = settings.main.model.clone();
    let client = Arc::new(llm::Client::new(settings.main));
    let small = settings.small.map(|c| Arc::new(llm::Client::new(c)));
    let ctx = Arc::new(tools::Ctx::new(cwd.clone()));
    let mut agent = Agent::new(client, ctx, AgentOpts {
        yes: auto, allow_outside: common.allow_outside || auto, guard, readonly: false, quiet: false, format: common.format,
        limits: Limits { max_time, max_steps: common.max_steps, max_tokens: common.max_tokens }, small, thinking,
    });
    // session: explicit id > --continue (latest for this cwd) > new
    let sid = match (&common.session, common.cont) {
        (Some(s), _) => Some(s.clone()),
        (None, true) => session::latest_for_cwd(&cwd),
        _ => None,
    };
    let resume = sid.is_some();
    agent.attach_session(session::Session::open_or_create(sid.as_deref(), &cwd, &model_name)?, resume);

    match prompt {
        Some(p) => {
            match agent.turn(&p).await {
                Ok(o) => Ok(match o.stop { Stop::Done => 0, Stop::Limit(_) => 3, Stop::Interrupted => 130 }),
                Err(e) => {
                    agent.emit_error("api", &e.to_string());
                    if common.format == Format::Default { eprintln!("error: {e}"); }
                    Ok(1)
                }
            }
        }
        None => { repl::run(agent, resume).await?; Ok(0) }
    }
}

fn list_sessions() -> anyhow::Result<i32> {
    let v = session::list();
    if v.is_empty() { println!("no sessions"); }
    for i in v.iter().take(50) {
        let age = i.mtime.elapsed().map(|d| d.as_secs()).unwrap_or(0);
        println!("{}  {:>5} msgs  {:>6} ago  {}  {}", i.id, i.messages, fmt_age(age), i.model, i.cwd);
    }
    Ok(0)
}

fn fmt_age(s: u64) -> String {
    match s { 0..=59 => format!("{s}s"), 60..=3599 => format!("{}m", s / 60), 3600..=86399 => format!("{}h", s / 3600), _ => format!("{}d", s / 86400) }
}

fn upgrade() -> anyhow::Result<i32> {
    let repo = std::env::var("ZCODE_REPO").unwrap_or_else(|_| env!("ZCODE_DEFAULT_REPO").to_string());
    let url = format!("https://raw.githubusercontent.com/{repo}/main/install.sh");
    println!("upgrading from {repo} …");
    let st = std::process::Command::new("sh").arg("-c").arg(format!("curl -fsSL '{url}' | ZCODE_REPO='{repo}' bash")).status()?;
    Ok(st.code().unwrap_or(1))
}
