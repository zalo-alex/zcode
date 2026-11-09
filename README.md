<div align="center">

# ⚡ zcode

**A coding agent for your terminal that starts before you blink.**

One 8 MB binary. 2 ms startup. ~30 MB of RAM. Any OpenAI-compatible model.

![license](https://img.shields.io/badge/license-MIT-blue?style=flat-square)
![rust](https://img.shields.io/badge/built%20with-Rust-dea584?style=flat-square&logo=rust&logoColor=white)
![platform](https://img.shields.io/badge/linux-x86__64%20%7C%20aarch64-black?style=flat-square&logo=linux&logoColor=white)
![tests](https://img.shields.io/badge/tests-107%20passing-brightgreen?style=flat-square)

[Install](#-install) · [Quick start](#-quick-start) · [Features](#-features) · [Benchmarks](#-benchmarks) · [Docs](#-docs)

</div>

```text
$ zcode
zcode 0.3.0 · deepseek-v4-1-flash-free · ~/projects/api
session ses_6aece2ed815bce · auto-approve

› the login test fails, fix it
▸ grep login
▸ read tests/test_login.py
▸ read app/auth.py
· thinking
  the token check uses < instead of <=, so a token that expires "now" is rejected…
▸ edit app/auth.py
  - if token.exp < now:
  + if token.exp <= now:
▸ bash pytest -q tests/test_login.py
Fixed. `auth.py` rejected tokens that expire exactly now; the test passes.
```

## ✨ Why zcode

Most agent CLIs ship a browser engine's worth of runtime. zcode is a single native binary that does the same job with a fraction of the weight, on the same model.

| | **zcode** | opencode¹ |
|---|---|---|
| Startup (`--version`) | **~2 ms** | ~125–220 ms |
| Memory while working | **~30–45 MB** | ~410 MB |
| Fixed prompt cost per request | **~1.5k tokens** | ~8.5k tokens |
| Tokens per solved task² | **~11–14k** | ~55k |
| Task pass rate² | 97–98 % | 98–100 % |

<sub>¹ Measured side by side on the same model (`deepseek-v4-1-flash-free`) through a metering proxy, see [Benchmarks](#-benchmarks). ² 20 coding tasks, hidden checkers, 2–3 runs each: a statistical tie on quality, a large gap on cost.</sub>

## 📦 Install

```bash
curl -fsSL https://raw.githubusercontent.com/zalo-alex/zcode/main/install.sh | bash
```

Linux x86_64 and aarch64. The binary only needs glibc 2.34+ (Debian 12 and newer). Update later with `zcode upgrade`.

<details>
<summary>Build from source</summary>

```bash
export PATH=$HOME/.cargo/bin:$PATH
cargo build --release          # → target/release/zcode
cargo test                     # unit + integration tests against a fake OpenAI server
```

`scripts/build-bookworm.sh` builds a bookworm-compatible release binary with Docker.
</details>

## 🚀 Quick start

zcode reuses your existing opencode provider config. No config? Create `~/.config/zcode/config.json`:

```json
{
  "model": "mine/my-model",
  "provider": {
    "mine": {
      "options": { "baseURL": "https://api.example.com/v1", "apiKey": "sk-…" },
      "models": { "my-model": { "limit": { "context": 128000, "output": 16384 } } }
    }
  }
}
```

```bash
zcode                                  # interactive session
zcode run "add input validation to signup()"
zcode run --format json --auto "…"     # machine-readable, no prompts
zcode --continue                       # pick up where you left off in this directory
```

## 🧰 Features

<table>
<tr>
<td width="50%" valign="top">

### 🧠 Real agent loop
Streaming replies, parallel read-only tools, a sub-agent for broad searches, todo tracking, automatic context compaction, and nudges when the model stalls or wanders.

### 💭 Thinking, visible
The model's reasoning streams dimmed on stderr (answer stays clean on stdout). Toggle with `--thinking` / `--no-thinking` or `/thinking`.

### 🔁 Sessions that survive anything
Append-only and crash-safe. `--continue` resumes the latest session *for the current directory*; `--session <id>` picks your own.

</td>
<td width="50%" valign="top">

### 🛡️ Safe by default
Mutating tools ask first. A prompt-injection guard locks down write/edit/bash for the rest of a turn if a file or web page tries to instruct the agent.

### 🖥️ Built for automation
`--format json` emits one event per line (opencode-compatible shape), with stable exit codes and per-turn time, step and token limits.

### 🔌 Bring your own model
Any OpenAI-compatible `/chat/completions` endpoint with streaming and tool calls. Retries, stall detection and overflow recovery are built in.

</td>
</tr>
</table>

### Tools

| Tool | What it does |
|---|---|
| `read` `write` `edit` `multiedit` | Line-numbered reads; exact edits with an indentation-tolerant fallback; atomic multi-edits |
| `glob` `grep` `ls` | `.gitignore`-aware search and listing |
| `bash` | Persistent working directory, timeouts, process-group kill, **background jobs** |
| `bash_output` `bash_kill` | Read and stop background jobs (dev servers, watchers) |
| `webfetch` | Fetch a URL as clean text |
| `task` | Read-only sub-agent for exploration |
| `todowrite` `todoread` | Plan and track multi-step work |

### Inside the REPL

`/help` `/thinking` `/compact` `/clear` `/sessions` `/status` `/todo` `/cost` `/model` `/exit` — and `Ctrl-C` interrupts the model *or* a running tool. Lines typed while a turn is running are queued and executed in order.

## 🔒 Safety model

- **Permissions:** `write`, `edit` and `bash` ask `y / N / always`. `--auto` skips prompts. Obvious disasters (`rm -rf /`, `mkfs`, …) are always blocked.
- **Confinement:** writes outside the working directory need `--auto` or `--allow-outside`.
- **Sandbox (tier 1, default):** the bash child runs under Landlock + seccomp: it only sees the project, system dirs and toolchains read-only, a private tmp, no `$HOME` secrets, no network unless `--net`, no `ptrace`/`mount`. File tools are path-checked with `openat2(RESOLVE_BENEATH)`. `--sandbox none` opts out; unsupported kernels fail closed (see `docs/technical/PROTOCOL.md`).
- **Daemon (opt-in, `zcode serve`):** one resident process hosting many instances on a shared bus and model-connection pool; clients (including `zcode run --auto`) create, attach and detach by id. See `docs/technical/DAEMON.md`.
- **Tier 2 (opt-in, `--sandbox container`):** one rootless crun container per agent with its own root filesystem, namespaces and network (`pasta` or `slirp4netns`); only the project dir is shared (read-only unless assigned). Images: `zcode image import`, or `zcode image pull alpine` (build with `--features oci` for the in-process registry client). See `docs/technical/TIER2.md`.
- **Swarm (opt-in, `--max-workers N`):** the lead can `spawn` workers that run in the same process, each with its own sandbox, history and write paths, and coordinate through `send` / `wait`. Budgets cover the whole swarm.
- **Injection guard:** instruction-like text in files, web pages or command output (“ignore the user…”) is tagged as untrusted data, and mutating tools are denied for the rest of that turn. Tune with `--guard ask|deny|warn|off`.
- **Runaway protection:** identical-call detection, an exploration-loop breaker, and limits on time, steps and tokens (`--max-time`, `--max-steps`, `--max-tokens`). Hitting a limit exits with code `3`.

## 📊 Benchmarks

The [`bench/`](bench) folder compares zcode with opencode on **20 agentic coding tasks** (easy → hard, hidden test-based checkers) and **16 prompt cases** (reasoning, strict formats, executed codegen, long-context needle, multi-turn memory, tool choice, prompt injection). Both tools hit the same model through a local metering proxy that records requests, tokens and time-to-first-token.

```bash
cd bench && python3 -m venv venv && source venv/bin/activate && pip install -r requirements.txt
python bench.py --runs 3 --parallel 8
```

Measured on one machine against one free model: zcode matched opencode on task quality while using ~5× fewer tokens and ~10× less memory, and it resisted the prompt-injection case that opencode failed. Small samples and a single model, so treat pass-rate differences of a task or two as noise. Details in [`bench/README.md`](bench/README.md).

## 📚 Docs

- **[User guide](docs/user/README.md)**: install, everyday use, the sandbox, teams of agents, containers and the daemon, with commands and their output.
- **[Technical docs](docs/technical/README.md)**: every flag, config key, environment variable, JSON event, exit code, the guest and daemon protocols, and known gaps.
- **Config layers** (later wins): `~/.config/opencode/opencode.json` → `~/.config/zcode/config.json` → env `OPENCODE_CONFIG_CONTENT` → env `ZCODE_CONFIG_CONTENT`.
- **Instructions:** `~/.config/zcode/AGENTS.md`, `~/.config/opencode/AGENTS.md` and the nearest project `AGENTS.md` / `CLAUDE.md` are all added to the system prompt.

## 🧪 Development

```bash
cargo test                      # 57 unit + integration tests
cargo clippy --all-targets      # clean
scripts/tmux-e2e.sh             # end-to-end REPL test driven through tmux
```

CI runs clippy and the test suite on every push; tagging `v*` builds and publishes release binaries (x86_64 and aarch64, built against Debian 12 for broad compatibility).

Not implemented yet: MCP servers, LSP, image attachments, a full-screen TUI.

## 📄 License

[MIT](LICENSE)
