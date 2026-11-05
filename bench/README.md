# zcode vs opencode benchmark

```bash
cd bench
python3 -m venv venv && source ./venv/bin/activate && pip install -r requirements.txt
(cd .. && PATH=$HOME/.cargo/bin:$PATH cargo build --release)

python bench.py                                   # everything, 3 runs each (slow: real LLM calls)
python bench.py --suite startup overhead          # no-LLM-quality suites, ~10s
python bench.py --suite tasks --levels hard --runs 2
python bench.py --suite prompts --cases json_only needle_long_ctx
python bench.py --tools zcode                     # one tool only
```

Reports go to `results/<timestamp>.{md,json}` (json has every raw run incl. failure output tails).

## What is measured
All LLM traffic of both tools is routed through `proxy.py` (local metering proxy → the real endpoint), so
both are measured identically and use the same upstream model from `~/.config/opencode/opencode.json`.

| suite | what | metrics |
|-------|------|---------|
| startup | `--version` x N | median/min/p95 latency, RSS |
| overhead | request for "Say only PONG." | system-prompt chars, tool count, tool-schema chars, request bytes, prompt tokens, TTFT |
| tasks (20) | agentic coding in a fresh temp dir; **hidden** checkers (never visible to the agent) | pass rate by level, wall s, CPU s, peak RSS, #LLM requests, tokens, tokens/pass, s/pass |
| prompts (16) | reasoning, strict format, codegen executed in a sandbox, long-context needle, multi-turn memory/correction (`--resume`/`--continue`), tool-choice (needs tool vs doesn't), prompt-injection resistance, vague/ambiguous prompts | pass rate by category, TTFT, tokens |

Task levels: **easy** (create, fix bug, search, rename, vague prompt) · **medium** (multi-file bugs, implement from spec, 3000-function file bug, 400-file repo search, CSV report, git workflow, add function + tests, class refactor) · **hard** (CLI from spec, error recovery, one-line-change constraint, performance optimization, expression parser, deep JSON merge).

## Fairness notes
- `PWD` is set per run (opencode trusts `$PWD`, otherwise it edits the harness directory).
- opencode runs `--standalone --auto` (private server, memory counted); zcode runs `--yes`. zcode sessions isolated via `XDG_DATA_HOME`.
- Token counts use upstream `usage`; some endpoints omit it, in which case counts fall back to request-bytes/4 (prompt) and output-chars/4 (completion): good for relative comparison, not exact billing.
- opencode also issues auxiliary requests (e.g. title generation); they are counted in totals, while the overhead table uses the main agent request.
- Model output is non-deterministic: use `--runs 3+` and compare pass rates, not single runs.
- Wall time is dominated by upstream latency; CPU s and RSS isolate the client cost.

## Adding cases
`tasks.py`: add `Task(name, level, prompt, setup(dir), check(dir, stdout))`. `prompts.py`: add `PromptCase(name, category, turns, check(dir, outputs), setup)`.
