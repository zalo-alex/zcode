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

### Exit codes
`0` finished · `1` error (API, config, session id…) · `3` stopped by a limit (time, steps, token budget, exploration loop) · `130` interrupted (Ctrl-C) · `143` SIGTERM/SIGHUP.

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
