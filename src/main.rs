mod agent;
mod client;
mod config;
mod context;
mod events;
mod guard;
mod llm;
mod peer;
mod permission;
mod prompt;
mod repl;
mod serve;
mod session;
mod guest_embed;
mod swarm;
mod style;
use zcode_tools as tools;

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
#[allow(clippy::large_enum_variant)]
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
    /// Run the daemon: one resident process hosting many agent instances (opt-in)
    Serve,
    /// Manage the images used by `--sandbox container`
    Image {
        #[command(subcommand)]
        cmd: ImageCmd,
    },
}

#[derive(Subcommand)]
enum ImageCmd {
    /// Import a rootfs tarball (.tar, .tar.gz/.xz/.zst) or directory
    Import { src: std::path::PathBuf, #[arg(long)] name: String },
    /// List imported images
    Ls,
    /// Remove an image
    Rm { name: String },
    /// Pull an OCI image from a registry (in-process with the `oci` feature, else via skopeo + umoci) and import it
    Pull { reference: String, #[arg(long)] name: Option<String>,
        /// os/arch[/variant] to pick from a multi-arch image (default: this machine; needs the `oci` feature)
        #[arg(long)] platform: Option<String> },
    /// Delete cached image layers that no pulled image uses (needs the `oci` feature)
    Gc,
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
    /// Sandbox tier: landlock (default, tier 1), none (tier 0), warn (tier 1 or unconfined with a warning), container (tier 2)
    #[arg(long, value_enum, global = true)]
    sandbox: Option<SandboxArg>,
    /// Fail instead of running with a weaker sandbox than requested
    #[arg(long, global = true)]
    require_sandbox: bool,
    /// Extra read-only path for the sandbox (repeatable)
    #[arg(long = "allow-read", global = true)]
    allow_read: Vec<std::path::PathBuf>,
    /// Extra read-write path for the sandbox (repeatable)
    #[arg(long = "allow-write", global = true)]
    allow_write: Vec<std::path::PathBuf>,
    /// Allow network access (bash sockets, webfetch, websearch); default off under the sandbox
    #[arg(long, global = true)]
    net: bool,
    /// Enable the swarm: the lead may spawn up to N workers (0 = off, default; config `swarm.max_workers`)
    #[arg(long, global = true)]
    max_workers: Option<usize>,
    /// Keep container state (upper layers) after the run, for debugging
    #[arg(long, global = true)]
    keep_state: bool,
    /// Fail instead of skipping cgroup limits when cgroup v2 is not delegated
    #[arg(long, global = true)]
    require_limits: bool,
    /// Show worker progress (one prefixed line per step, no interleaved streaming)
    #[arg(long, global = true)]
    show_workers: bool,
    /// Interactive: join a group of agents through the daemon (`zcode serve`); agents in a group can message each other
    #[arg(long, global = true)]
    group: Option<String>,
    /// Your agent's name in the group, so others can address it (default zcode-<pid>)
    #[arg(long, global = true, requires = "group")]
    agent: Option<String>,
    /// Interactive: attach to an instance that already exists in the daemon (created by another program); leaving detaches only
    #[arg(long, global = true, value_name = "INSTANCE", conflicts_with = "group")]
    attach: Option<String>,
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

fn main() {
    // `zcode __ns ...` is the single-threaded tier 2 supervisor: it must run before any runtime exists
    if let Some(c) = tools::container::maybe_supervisor() { std::process::exit(c); }
    amain()
}

#[tokio::main]
async fn amain() {
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
    if matches!(cli.cmd, Some(Cmd::Serve)) { return serve::run().await; }
    install_signal_handlers();
    let (common, prompt, is_run) = match cli.cmd {
        Some(Cmd::Sessions) => return list_sessions(),
        Some(Cmd::Upgrade) => return upgrade(),
        Some(Cmd::Image { cmd }) => return image_cmd(cmd).await,
        Some(Cmd::Serve) => unreachable!(),
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
    // `--attach`: the agent already exists in the daemon with its own model, sandbox and limits: nothing to load here
    if let Some(inst) = &common.attach {
        if is_run || prompt.is_some() { anyhow::bail!("--attach is for the interactive mode: run `zcode --attach NAME` without a message"); }
        return peer::attach(inst.clone()).await;
    }
    if let Some(d) = &common.cwd { std::env::set_current_dir(d)?; }
    let cwd = std::env::current_dir()?.canonicalize()?;
    let settings = config::Settings::load(common.model.as_deref())?;
    let auto = common.auto || settings.auto;
    let max_time = match common.max_time { Some(0) => None, Some(s) => Some(Duration::from_secs(s)), None => is_run.then_some(Duration::from_secs(1200)) };
    let guard = common.guard.unwrap_or(if auto || !std::io::stdin().is_terminal() { GuardMode::Deny } else { GuardMode::Ask });
    let thinking = if common.no_thinking { false } else if common.thinking { true } else { settings.thinking.unwrap_or(!is_run) };
    let model_name = settings.main.model.clone();
    // `--group`: the interactive agent lives in the daemon, next to the other agents of its group
    if let Some(group) = &common.group {
        if is_run || prompt.is_some() { anyhow::bail!("--group is for the interactive mode: run `zcode --group NAME --auto` without a message"); }
        if !auto { anyhow::bail!("--group runs your agent inside the daemon, which cannot ask for permission: add --auto (commands still run in the sandbox)"); }
        let (sandbox, policy, max_workers) = daemon_request(&common, &settings, &cwd, auto)?;
        let name = common.agent.clone().unwrap_or_else(|| format!("zcode-{}", std::process::id()));
        return peer::run(peer::Join { settings: &settings, cwd: &cwd, group: group.clone(), name, sandbox, require_sandbox: common.require_sandbox, policy, max_workers,
            max_steps: common.max_steps, max_tokens: common.max_tokens, max_time: max_time.map(|d| d.as_secs()), session: common.session.clone() }).await;
    }
    // a running daemon takes over `zcode run` (auto-approve runs only: a daemon cannot prompt a terminal); otherwise embedded
    if is_run && auto && !common.cont && common.session.is_none() && std::env::var_os("ZCODE_NO_DAEMON").is_none() && let Some(p) = &prompt {
        let (mode_s, pol, mw) = daemon_request(&common, &settings, &cwd, auto)?;
        let req = client::Req { settings: &settings, cwd: &cwd, sandbox: mode_s, require_sandbox: common.require_sandbox, policy: pol, max_time: max_time.map(|d| d.as_secs()),
            max_steps: common.max_steps, max_tokens: common.max_tokens, max_workers: mw, format: common.format, thinking, prompt: p };
        if let Some(code) = client::try_run(req).await { return Ok(code); }
    }
    let client = Arc::new(llm::Client::new(settings.main));
    let small = settings.small.map(|c| Arc::new(llm::Client::new(c)));
    let (client_c, small_c) = (client.clone(), small.clone());
    let mode = sandbox_mode(&common, &settings.raw)?;
    tools::container::configure(tools::container::ContainerCfg::from_config(&settings.raw, guest_embed::GUEST, common.keep_state, common.require_limits).map_err(anyhow::Error::msg)?);
    let policy = build_policy(&common, &settings.raw, &cwd, auto);
    let net = policy.net;
    let (sandbox, warning) = tools::open(mode, policy.clone(), auto, common.require_sandbox)?;
    let lead_sandbox = sandbox.clone();
    let lead_tier = sandbox.tier();
    let idmap_full = settings.raw["container"]["idmap"].as_str() == Some("full");
    if let Some(w) = warning { eprintln!("{w}"); }
    let ctx = Arc::new(tools::Ctx::with_sandbox(sandbox, net || mode == tools::Mode::None));
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

    let sb = agent.ctx.sandbox.clone();
    let swarm_cfg = settings.raw.get("swarm");
    let max_workers = common.max_workers.or_else(|| swarm_cfg.and_then(|s| s.get("max_workers")).and_then(|v| v.as_u64()).map(|n| n as usize)).unwrap_or(0);
    let mut swarm = None;
    if max_workers > 0 {
        let g = |k: &str, d: u64| swarm_cfg.and_then(|s| s.get(k)).and_then(|v| v.as_u64()).unwrap_or(d);
        let cfg = swarm::SwarmCfg { max_workers, max_depth: g("max_depth", 1) as u32, pair_cap: g("pair_cap", 40) as u32, total_cap: g("total_cap", 400) as u32,
            chain_cap: g("chain_cap", 16) as u32, concurrency: g("concurrency", 4) as usize, show_workers: common.show_workers, json: common.format == Format::Json, unbounded: false };
        let sc = swarm::SpawnCtx { client: client_c, small: small_c, raw: settings.raw.clone(), policy, mode, auto, require: common.require_sandbox,
            session_base: agent.sid().to_string(), model: model_name.clone(), lead_tier, idmap_full, limits: agent.limits.clone(), format: common.format };
        let (sw, rx) = swarm::Swarm::new(cfg, sc, "lead", lead_sandbox);
        agent.join_swarm_as_lead(sw.clone(), rx);
        swarm = Some(sw);
    }
    let res: anyhow::Result<i32> = match prompt {
        Some(p) => {
            let first = agent.turn(&p).await;
            match settle(&mut agent, first).await {
                Ok(o) => Ok(match o.stop { Stop::Done => 0, Stop::Limit(_) => 3, Stop::Interrupted => 130 }),
                Err(e) => {
                    agent.emit_error("api", &e.to_string());
                    if common.format == Format::Default { eprintln!("error: {e}"); }
                    Ok(1)
                }
            }
        }
        None => repl::run(agent, resume).await.map(|_| 0),
    };
    if let Some(sw) = &swarm { sw.shutdown().await; }
    sb.close().await;
    res
}

/// One-shot runs: when the lead answers while workers are still active, keep going until they settle.
async fn settle(agent: &mut Agent, first: anyhow::Result<agent::Outcome>) -> anyhow::Result<agent::Outcome> {
    let mut r = first;
    while let (Ok(o), Some(sw)) = (&r, agent.swarm.clone()) {
        if o.stop != Stop::Done || !sw.active(&agent.name) { break; }
        sw.wait(&agent.name).await;
        r = agent.turn("[system] Worker messages arrived or workers are still running. Handle their messages above, finish any coordination, then give your final answer.").await;
    }
    r
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

#[derive(Clone, Copy, clap::ValueEnum)]
enum SandboxArg { None, Landlock, Warn, Container }

/// What a daemon request carries from the command line: sandbox level, policy and worker count (same for `run` and `--group`).
fn daemon_request(common: &Common, settings: &config::Settings, cwd: &std::path::Path, auto: bool) -> anyhow::Result<(&'static str, serde_json::Value, usize)> {
    let mode_s = match sandbox_mode(common, &settings.raw)? { tools::Mode::None => "none", tools::Mode::Landlock => "landlock", tools::Mode::Warn => "warn", tools::Mode::Container => "container" };
    let abs = |v: &Vec<std::path::PathBuf>| v.iter().map(|p| if p.is_absolute() { p.clone() } else { cwd.join(p) }).collect::<Vec<_>>();
    let pol = serde_json::json!({"read": abs(&common.allow_read), "write": abs(&common.allow_write),
        "net": common.net || settings.raw.get("net").and_then(|v| v.as_bool()).unwrap_or(false), "outside": common.allow_outside || auto});
    let mw = common.max_workers.or_else(|| settings.raw.get("swarm").and_then(|s| s.get("max_workers")).and_then(|v| v.as_u64()).map(|n| n as usize)).unwrap_or(0);
    Ok((mode_s, pol, mw))
}

fn sandbox_mode(common: &Common, raw: &serde_json::Value) -> anyhow::Result<tools::Mode> {
    let m = match (common.sandbox, raw.get("sandbox").and_then(|v| v.as_str())) {
        (Some(SandboxArg::None), _) | (None, Some("none")) => tools::Mode::None,
        (Some(SandboxArg::Landlock), _) | (None, Some("landlock")) | (None, None) => tools::Mode::Landlock,
        (Some(SandboxArg::Warn), _) | (None, Some("warn")) => tools::Mode::Warn,
        (Some(SandboxArg::Container), _) | (None, Some("container")) => tools::Mode::Container,
        (None, Some(o)) => anyhow::bail!("config `sandbox` must be none|landlock|warn|container, got `{o}`"),
    };
    Ok(m)
}

fn expand(p: &str) -> std::path::PathBuf {
    match p.strip_prefix("~/") { Some(r) => tools::home().join(r), None => p.into() }
}

fn build_policy(common: &Common, raw: &serde_json::Value, cwd: &std::path::Path, auto: bool) -> tools::Policy {
    let abs = |p: &std::path::PathBuf| if p.is_absolute() { p.clone() } else { cwd.join(p) };
    let mut p = tools::Policy::new(cwd.to_path_buf());
    p.read.extend(common.allow_read.iter().map(abs));
    p.write.extend(common.allow_write.iter().map(abs));
    p.net = common.net || raw.get("net").and_then(|v| v.as_bool()).unwrap_or(false);
    p.outside = common.allow_outside || auto;
    let list = |k: &str| raw.get(k).and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect::<Vec<_>>());
    if let Some(t) = list("toolchain_dirs") { p.toolchain = t.iter().map(|s| expand(s)).collect(); }
    if let Some(e) = list("sandbox_env") { p.env = e; }
    p
}

async fn image_cmd(cmd: ImageCmd) -> anyhow::Result<i32> {
    use tools::container as c;
    match cmd {
        ImageCmd::Import { src, name } => { println!("{}", c::image_import(&src, &name).map_err(anyhow::Error::msg)?); }
        ImageCmd::Ls => {
            let v = c::image_list();
            if v.is_empty() { println!("no images (import one with `zcode image import <tar|dir> --name <n>`)"); }
            for (n, p) in v { println!("{n}  {}", p.display()); }
        }
        ImageCmd::Rm { name } => { c::image_remove(&name).map_err(anyhow::Error::msg)?; println!("removed {name}"); }
        ImageCmd::Gc => {
            #[cfg(feature = "oci")]
            { println!("{}", c::image_gc().map_err(anyhow::Error::msg)?); }
            #[cfg(not(feature = "oci"))]
            anyhow::bail!("`zcode image gc` needs a build with the `oci` feature (cargo build --features oci)");
        }
        ImageCmd::Pull { reference, name, platform } => {
            let name = name.unwrap_or_else(|| reference.rsplit('/').next().unwrap_or("image").split([':', '@']).next().unwrap_or("image").to_string());
            #[cfg(feature = "oci")]
            println!("{}", c::image_pull(&reference, &name, platform.as_deref()).await.map_err(anyhow::Error::msg)?);
            #[cfg(not(feature = "oci"))]
            println!("{}", pull_with_skopeo(&reference, &name, platform.as_deref())?);
        }
    }
    Ok(0)
}

/// Without the `oci` feature: skopeo + umoci do the registry work, `image import` the rest.
#[cfg(not(feature = "oci"))]
fn pull_with_skopeo(reference: &str, name: &str, platform: Option<&str>) -> anyhow::Result<String> {
    if platform.is_some() { anyhow::bail!("--platform needs a build with the `oci` feature; this build pulls with skopeo + umoci"); }
    for t in ["skopeo", "umoci"] {
        if std::process::Command::new(t).arg("--version").stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).status().is_err() {
            anyhow::bail!("`zcode image pull` needs `{t}` in PATH (this build has no in-process registry client; build with `--features oci`)");
        }
    }
    let tmp = std::env::temp_dir().join(format!("zcode-pull-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let oci = format!("oci:{}:latest", tmp.join("oci").display());
    let ok = |s: std::io::Result<std::process::ExitStatus>| s.map(|s| s.success()).unwrap_or(false);
    if !ok(std::process::Command::new("skopeo").args(["copy", &format!("docker://{reference}"), &oci]).status()) { anyhow::bail!("skopeo copy failed"); }
    if !ok(std::process::Command::new("umoci").args(["unpack", "--rootless", "--image", &format!("{}:latest", tmp.join("oci").display())]).arg(tmp.join("bundle")).status()) { anyhow::bail!("umoci unpack failed"); }
    let r = tools::container::image_import(&tmp.join("bundle/rootfs"), name).map_err(anyhow::Error::msg);
    let _ = std::fs::remove_dir_all(&tmp);
    r
}
