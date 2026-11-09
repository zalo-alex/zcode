# zcode CLI protocol

Everything another program (a wrapper, a bot, a script) needs to drive zcode.

## Invocation

| Command | What |
|---|---|
| `zcode` | interactive REPL (prompt `› `) |
| `zcode run [flags] <message…>` | one non-interactive turn, then exit. Message from stdin when no argument and stdin is piped |
| `zcode -p "msg"` / `zcode msg…` | same as `run` (kept for compatibility) |
| `zcode sessions` | list saved sessions |
| `zcode upgrade` | re-run `install.sh` from `$ZCODE_REPO` (latest release) |
| `zcode serve` | opt-in daemon hosting many agent instances, see [DAEMON.md](DAEMON.md); `zcode run --auto` becomes a thin client when it is running |
| `zcode image import <tar or dir> --name n` / `ls` / `rm n` / `pull <ref> [--name n] [--platform os/arch]` / `gc` | images for `--sandbox container`, see [TIER2.md](TIER2.md). `pull` and `gc` are in-process with the `oci` cargo feature, otherwise `pull` needs skopeo + umoci |

Flags (accepted before or after `run`):

| Flag | Meaning |
|---|---|
| `-m, --model provider/model` | model; provider must exist in the config (`provider/org/model` splits at the first `/`) |
| `-y, --auto` (alias `--yes`) | no permission prompts; also lifts the outside-cwd restriction. Implied by config `permission: {"*":"allow"}` |
| `-c, --continue` (aliases `-r`, `--resume`) | continue the latest session **of this working directory** |
| `-s, --session ID` | use or create session `ID` (`[A-Za-z0-9_-]{1,64}`) |
| `--format default\|json` | human output, or one JSON event per line on stdout |
| `-C, --cwd DIR` | working directory |
| `--allow-outside` | allow write/edit outside the working directory |
| `--guard ask\|deny\|warn\|off` | reaction to prompt injection (see below). Default `ask` on a terminal, `deny` with `--auto` or without a terminal |
| `--thinking` / `--no-thinking` | show / hide the model's reasoning (`reasoning_content` stream). Default: on in the REPL, off for `run`. Config key `thinking: true|false`. REPL: `/thinking` toggles |
| `--max-time SECS` | wall-clock limit per turn, `0` = none. Default 1200 for `run`, none in the REPL |
| `--max-steps N` | model steps per turn (default 60) |
| `--max-tokens N` | cumulative prompt tokens per turn (default 400000) |
| `--sandbox none\|landlock\|warn\|container` | isolation tier, see *Sandbox* below. Default `landlock` (tier 1). Config key `sandbox` |
| `--require-sandbox` | fail instead of running with a weaker sandbox than requested |
| `--allow-read PATH` / `--allow-write PATH` | extra read-only / read-write sandbox paths (repeatable) |
| `--net` | allow network (bash `AF_INET`/`AF_INET6`, `webfetch`). Off by default under the sandbox. Config key `net` |
| `--max-workers N` | enable the swarm with up to N workers (see *Swarm*) |
| `--keep-state` / `--require-limits` | tier 2: keep container upper layers after the run / fail instead of skipping cgroup limits without delegation |
| `--show-workers` | print worker progress lines |
| `--group G` / `--agent NAME` | interactive only, needs `zcode serve` and `--auto`: run this terminal's agent inside the daemon, in group `G`, under the name `NAME` (default `zcode-<pid>`), so agents in other terminals of the group can message it and it them. See [DAEMON.md](DAEMON.md#interactive-agents-in-a-group-zcode---group) |

### Exit codes
`0` finished · `1` error (API, config, session id, a sandbox tier that cannot be enforced…) · `3` stopped by a limit (time, steps, token budget, exploration loop; with a swarm the budgets cover the whole swarm; through the daemon the same codes arrive in `turn_end.code`) · `130` interrupted (Ctrl-C / daemon `cancel`) · `143` SIGTERM/SIGHUP.

## Configuration

Layers, later wins, objects merged recursively:

1. `~/.config/opencode/opencode.json`
2. `~/.config/zcode/config.json` (`$XDG_CONFIG_HOME` honoured)
3. env `OPENCODE_CONFIG_CONTENT` (JSON)
4. env `ZCODE_CONFIG_CONTENT` (JSON)

Schema (opencode-compatible subset):

```json
{
  "model": "prov/model-id",
  "small_model": "prov/cheap-model",
  "permission": {"*": "allow"},
  "provider": {
    "prov": {
      "options": {"baseURL": "http://host:port/v1", "apiKey": "…"},
      "models": {"model-id": {"limit": {"context": 128000, "output": 16384}}}
    }
  }
}
```

`small_model` is used for context summaries. Any OpenAI-compatible `/chat/completions` endpoint with streaming and tool calls works (`npm`, `name`, `tool_call`, `attachment` keys are ignored).

Environment overrides: `ZCODE_MODEL` / `OPENCODE_MODEL`, `ZCODE_BASE_URL`, `ZCODE_API_KEY`, `ZCODE_STREAM_IDLE_SECS` (default 120), `ZCODE_BASH_TIMEOUT` (default 120), `ZCODE_INSTRUCTIONS` (extra instruction files, `:`-separated), `XDG_DATA_HOME`, `XDG_CACHE_HOME`, `NO_COLOR`.

Instruction files, all concatenated into the system prompt in this order: `~/.config/zcode/AGENTS.md`, `~/.config/opencode/AGENTS.md`, `$ZCODE_INSTRUCTIONS`, then the nearest `AGENTS.md` (or `CLAUDE.md`) from the working directory upwards.

## JSON events (`--format json`)

One object per line on stdout (nothing else is printed there). Shape follows opencode's `run --format json`.
Every event has `type`, `timestamp` (ms) and `sessionID`.

```json
{"type":"step_start","timestamp":1,"sessionID":"ses_…","part":{"type":"step-start"}}
{"type":"text","timestamp":2,"sessionID":"ses_…","part":{"type":"text","text":"complete assistant text of this step"}}
{"type":"tool_use","timestamp":3,"sessionID":"ses_…","part":{"type":"tool","tool":"bash","callID":"call_1",
  "state":{"status":"completed|error","input":{"command":"ls"},"output":"…"}}}
{"type":"step_finish","timestamp":4,"sessionID":"ses_…","part":{"type":"step-finish","reason":"tool-calls|stop","tokens":{"input":120,"output":30}}}
{"type":"reasoning","timestamp":2,"sessionID":"ses_…","part":{"type":"reasoning","text":"the model's reasoning for this step"}}
{"type":"error","timestamp":5,"sessionID":"ses_…","error":{"type":"limit|api|config|interrupted","message":"…"}}
```

* `reasoning` is only emitted with `--thinking` (or config `thinking: true`), once per step, before that step's `text`. Reasoning is never stored in the session or sent back to the model. In human mode it is printed dimmed on **stderr**, so stdout stays the answer only.
* `text` arrives once per step, after the model finished it (not token by token).
* `error.type`: `limit` (exit 3), `interrupted` (130), `api` (HTTP/stream failure, 1), `config` (setup failure, `sessionID` may be empty, 1).
* Token counts are the upstream's `usage`, or an estimate (bytes/4) when the upstream sends none.

## Sessions

`$XDG_DATA_HOME/zcode/sessions/<id>.jsonl` (default `~/.local/share/zcode/sessions`). Line 1 is a header
`{"zcode_session":{"id","cwd","model","created"}}`, then one OpenAI chat message per line. Appended after every
message, so a killed process loses nothing. The system prompt is rebuilt on every start (fresh instructions); compaction rewrites the file.
`--session ID` on an unknown id creates it, so a caller can choose its own ids (one per chat room, say).

## Tools

`read write edit multiedit ls glob grep bash bash_output bash_kill webfetch task todowrite todoread`

* `bash`: `sh -c`, default timeout 120 s (kills the process group), **the working directory persists between calls**,
  output goes through a temp file so `cmd &` neither hangs the call nor dies of SIGPIPE.
  `background: true` runs the command detached (log in `~/.cache/zcode/jobs/`), returns a job id; read it with
  `bash_output`, stop it with `bash_kill`. Background jobs outlive the turn and the zcode process.
* `edit`: exact match, falling back to a match that ignores indentation (replacement is re-indented); files must be read first.
* `task`: read-only sub-agent for broad exploration.
* Tool output over 30 KB is cut (head + tail) and the full text is saved under `~/.cache/zcode/tool-output/`.



## Swarm (`--max-workers N`)

Off by default (no extra tool schemas, no extra prompt tokens). With `--max-workers N` (or config `swarm.max_workers`) the lead gets three tools, and each agent is an async task with its own history, session `<session>.<agent>`, inbox, `Policy` and `Sandbox`, all in the one zcode process on an in-memory bus.

| Tool | Meaning |
|---|---|
| `spawn(name, prompt, write?, profile?)` | lead only (`max_depth`, default 1: workers cannot spawn). Returns at once. Names are unique (`[A-Za-z0-9_-]{1,32}`, not `lead`). `write` = project paths the worker may modify (created on the host if missing, must be inside the project root); a worker without `write` is read-only. `profile` names an entry of config `profiles` |
| `send(to, text)` | non-blocking; `to` is an agent name or `*`. Send summaries and file paths under the project dir, not file contents (private tmp dirs and host-only locations are not visible to tier 2 workers) |
| `wait()` | parks until a message arrives for the caller, or every agent is idle with no message in flight |

Delivery is push: before each model call an agent drains its inbox and appends `[from <name>] <text>` as a user turn. When a worker's turn ends its final answer is sent to its parent and it goes idle; a new message wakes it with its context intact. Peer messages are trusted but always labeled; file, web and command output stay untrusted, and the injection guard state is per agent (a tainted worker is locked, others are not). Workers run auto-approved inside their own sandbox (approving `spawn` approves that).

Guardrails (config `swarm`: `max_workers`, `max_depth`, `pair_cap` 40 messages per pair, `total_cap` 400, `chain_cap` 16 chained replies, `concurrency` 4 model requests at once): over-limit `send` returns a tool error. `--max-time`, `--max-steps`, `--max-tokens` bound the WHOLE swarm (every agent's steps and prompt tokens are summed; exit code 3); a profile may add `max_steps` / `max_tokens` for one worker: these are lifetime caps (a message that wakes an idle worker does not reset them), and a worker that reaches one reports `[stopped: worker step cap reached]` to its parent.

Profiles (config): `"profiles": {"build": {"net": true, "max_steps": 30}}`. Keys: `net`, `sandbox` (`none|landlock|warn|container`), `max_steps`, `max_tokens` (and, for tier 2, `mem cpus ports image`). The session tier (`--sandbox`) is the minimum: a profile may raise a worker's tier, never lower it. A tier 1 lead with `net=false` cannot connect to a worker's forwarded port (AF_INET is denied): test servers inside the worker, or give the lead `--net`.

JSON events gain an `agent` field (`lead` or the worker name) plus `spawn` (`agent, parent, tier, write, profile`), `message` (`from, to, id, hops, text`) and `idle` (`agent`). REPL: `/agents` lists agents, tier, state and budgets; `/cost` is per agent and total; `--show-workers` prints one `[name] ...` line per worker step (never interleaved streaming).
## Sandbox

Every tool runs through a `Sandbox` (`crates/zcode-tools/src/sandbox.rs`): `exec` (bash, background jobs, kill), `fs()` (read/write/edit/multiedit/glob/grep/ls) and `close`. `webfetch` stays host-side and is gated by the agent's `net` policy. A `Policy { cwd, read, write, net, tmp, limits, env, toolchain, outside }` is built per agent.

| Tier | Flag | Isolation |
|---|---|---|
| 0 | `--sandbox none` | none (permission prompts only) |
| 1 | `--sandbox landlock` (default) | Landlock + seccomp on the bash child; path-checked file tools in-process |
| 2 | `--sandbox container` | one rootless OCI container per agent |

Fail closed: if the requested tier cannot be enforced zcode refuses to run with an actionable error. With `--auto` (or `--require-sandbox`) tier 1 on an unsupported kernel is an error; without `--auto` it runs unconfined with one warning. `--sandbox warn` always runs unconfined with one warning instead of failing; `--sandbox none` opts out.

**Tier 1 bash child** (set up in `pre_exec` from data built in the parent; the zcode process itself is never restricted):
* Landlock, best ABI (raw syscalls, no allocation after fork): read-only `/usr /bin /sbin /lib* /etc`, the project root and the toolchain dirs (`~/.cargo ~/.rustup ~/.cache ~/.npm`, config key `toolchain_dirs`); read-write: write paths (`--allow-write`, default the project root), a private per-agent `TMPDIR` (mode 0700 under `$XDG_RUNTIME_DIR/zcode` or `/tmp/zcode-<uid>`), `/dev/null /dev/zero /dev/urandom /dev/random /dev/tty`. The target of `/etc/resolv.conf` is readable too when it is a symlink out of `/etc` (systemd-resolved, WSL), otherwise no hostname resolves even with `--net`. Only the child's own `/proc/<pid>` and a few global `/proc` files are readable. Nothing else in `$HOME` (so never `~/.config/zcode`, `~/.config/opencode`, `~/.ssh`). TCP bind/connect is denied on ABI 4+ when `net=false`; ABI 6 also scopes signals and abstract unix sockets.
* seccomp denylist: `ptrace process_vm_readv/writev mount umount2 pivot_root unshare setns bpf keyctl add_key request_key kexec_load perf_event_open io_uring_* userfaultfd`; with `net=false` also `socket(AF_INET|AF_INET6)` (works on every kernel, covers UDP). `no_new_privs` is set.
* Environment: `env_clear()` plus `PATH HOME LANG LC_ALL TERM USER TMPDIR CARGO_HOME RUSTUP_HOME` and names listed in config `sandbox_env` (names containing `KEY`/`TOKEN`/`SECRET`/`PASSWORD` or starting `ZCODE_`/`OPENCODE_` are never passed).
* A policy whose roots contain an always-denied directory (e.g. running in `$HOME` with `~/.ssh`) is refused: Landlock cannot carve holes.
* Startup cost: the kernel probe and this policy check run when the session opens (so an unenforceable tier fails immediately); opening the rule fds and compiling the seccomp filter wait for the first `bash` call, so an idle session never pays for them.

A write path (`--allow-write`, a worker's `write`) may be a single file: that file is then writable and nothing next to it.

**Tier 1 file tools** run in the zcode process, so the policy is enforced in userland: every open is `openat2(RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS)` under an allowed root and the resulting fd is re-checked against the deny list (config dir, `~/.ssh`, `/proc`, `/sys`, and the rest of `$HOME` unless a deeper root covers it). `--allow-outside` / `--auto` widen the file tools to `/` minus the deny list; they do **not** widen bash (use `--allow-write`).

**Stale-edit detection**: each read records a content hash; `write`/`edit`/`multiedit` fail with `modified by another agent, re-read` if the file changed since.

Tier 2 (containers, images, guest protocol, profiles) is documented in [TIER2.md](TIER2.md), the daemon and its protocol in [DAEMON.md](DAEMON.md), and what is missing, partial or inherently limited in [KNOWN_GAPS.md](KNOWN_GAPS.md).

## Safety model

* Without `--auto`, write/edit/bash ask `y/N/always` on the terminal (denied without one). `rm -rf /`-style commands are always blocked.
* write/edit outside the working directory need `--auto` or `--allow-outside`.
* **Injection guard**: if a tool result (file, web page, command output) contains instruction-like text
  (“ignore the user”, “system instruction:”…), the result is tagged as untrusted data and, for the rest of that turn,
  write/edit/bash are `deny`-ed (never prompting) or confirmed by a human (`ask` on a terminal), per `--guard`.
* Loop protection: identical tool call repeated 3×, 12 read-only calls in a row (nudge) / 30 (stop), announced-but-not-made tool calls (nudge), step/time/token limits.

## Robustness

Retries (up to 4, backoff to 8 s) on 429/5xx, connection errors, stalled streams (no data for 120 s), empty replies and
upstream error text such as `[… upstream error …]`; context-overflow errors trigger pruning + summarisation (with `small_model`) and a retry;
output cut by the token limit is continued. Errors are printed verbatim (`error: LLM HTTP 429: …`) and the REPL keeps running.
