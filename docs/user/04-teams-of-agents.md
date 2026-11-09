# 4. Teams of agents

For a big job, zcode can split the work: the main agent (the **lead**) starts helpers (**workers**), gives each a task and the files it may change, and collects their reports. It is off by default and costs nothing when off.

```text
$ zcode run --auto --max-workers 2 "document and test cart.py"
```

`--max-workers N` is the only switch: the lead may start up to N workers. (Or set `"swarm": {"max_workers": 2}` in your config.)

## What it looks like

```text
$ zcode run --auto --max-workers 2 --show-workers "document and test cart.py"
▸ spawn Write a README for cart.py
▸ spawn Add a test for an empty cart
[docs] spawned (tier 1)
[tests] spawned (tier 1)
▸ wait
[tests] ▸ read test_cart.py
[docs] ▸ read README.md
[tests] ▸ edit test_cart.py
[docs] ▸ write README.md
[tests] done: Added an empty-cart assertion to test_cart.py.
[from tests] Added an empty-cart assertion to test_cart.py.
[docs] done: README.md now documents total() and its discount.
▸ wait
[from docs] README.md now documents total() and its discount.
Both workers are done: README.md documents total(), and test_cart.py covers an empty cart.
```

What happened:

- The lead used `spawn` twice. Each worker got a name, a task, and the **only file it may change** (`README.md` for `docs`, `test_cart.py` for `tests`).
- Workers ran at the same time, each with its own conversation.
- When a worker finished, its final answer was sent to the lead (`[from tests] …`). The lead's `wait` returns as soon as a message arrives.
- Without `--show-workers` you only see the lead's lines and the `[from …]` reports; the flag adds one prefixed line per worker step, never interleaved mid-sentence.

## The three tools

The lead gets three small extra tools; workers get `send` and `wait` (and no `spawn`):

| Tool | What it does |
|---|---|
| `spawn(name, prompt, write?, profile?)` | start a worker. `write` lists the files or folders it may change; `profile` picks extra settings (below) |
| `send(to, text)` | message one agent, or `*` for everyone. It does not wait for a reply |
| `wait()` | pause until a message arrives or everyone is idle |

Agents should send **summaries and file paths**, not whole files: every agent sees the same project, so a path is enough.

A finished worker goes idle and keeps its conversation. A new message wakes it up where it left off.

## Rules that keep it safe

- **Names are unique.**

  ```text
  $ zcode run --auto --max-workers 1 "start two workers named w1"
  ▸ spawn x
  ▸ spawn y
  Second spawn said:
  Error: agent `w1` already exists
  ```

- **Workers write only where they are told.** Everything else in the project is read-only for them, and a worker cannot start more workers.
- **Two agents never overwrite each other by accident.** If a file changed since an agent last read it, its edit fails with `modified by another agent, re-read`, and the agent re-reads and tries again.
- **Limits cover the whole team.** `--max-steps`, `--max-tokens` and `--max-time` are shared by every agent together (exit code 3 when reached). A [profile](#profiles-settings-for-one-worker) can also cap one worker.
- **No endless chatter.** A pair of agents can exchange at most 40 messages, the whole team 400, and one chain of replies 16; further `send`s return an error.
- **Messages from teammates are labelled** with who sent them, and file, web and command output is still treated as untrusted data. If one worker is tricked by something it read, only that worker is locked down.
- **At most 4 model requests run at once** across the team, so you do not hit rate limits.

## Seeing what is going on

In the interactive mode, `/agents` lists every agent, and `/cost` splits the tokens per agent:

```text
$ zcode --max-workers 2 --auto
zcode 0.3.0 · my-model · ~/shop
session ses_6af1a1010b6ccd · auto-approve
› document and test cart.py
▸ spawn Write a README for cart.py
▸ spawn Add a test for an empty cart
▸ wait
[from tests] Added an empty-cart assertion to test_cart.py.
▸ wait
[from docs] README.md now documents total() and its discount.
Both workers are done: README.md documents total(), and test_cart.py covers an empty cart.
› /agents
docs       tier 1 idle     queued 0 steps 3 tokens 4500/120 write /home/you/shop/README.md
lead       tier 1 running  queued 0 steps 4 tokens 6600/160
tests      tier 1 idle     queued 0 steps 3 tokens 4500/120 write /home/you/shop/test_cart.py
swarm budget: 10 steps, 15600 prompt tokens
› /cost
tokens in 6600 / out 160 · ctx ~629/128000
  docs: in 4500 / out 120
  lead: in 6600 / out 160
  tests: in 4500 / out 120
  swarm total: in 15600 / out 400
```

Each `/agents` line is: name, **tier** (how isolated it is, see [chapter 5](05-containers.md)), state (`running` or `idle`), messages waiting, steps and tokens used, and the files it may write.

## JSON events

With `--format json` every event names its `agent`, and three more event types appear:

```text
{"agent":"docs","parent":"lead","profile":null,"sessionID":"ses_6af1a349e07309","tier":1,"timestamp":1794220873244,"type":"spawn","write":["README.md"]}
{"agent":"tests","from":"tests","hops":1,"id":1,"sessionID":"ses_6af1a349e07309","text":"Added an empty-cart assertion to test_cart.py.","timestamp":1794220873372,"to":"lead","type":"message"}
{"agent":"tests","sessionID":"ses_6af1a349e07309","timestamp":1794220873372,"type":"idle"}
```

Each agent also keeps its own saved conversation, named `<session>.<agent>` (for example `ses_6af1a349e07309.tests`).

## Profiles: settings for one worker

The tool schema stays tiny; extra settings live in named **profiles** in your config, and the lead picks one by name:

```json
{
  "swarm": { "max_workers": 3 },
  "profiles": {
    "cheap":  { "max_steps": 10 },
    "online": { "net": true },
    "build":  { "net": true, "sandbox": "container", "mem": "2g", "cpus": 2 }
  }
}
```

| Key | Meaning |
|---|---|
| `net` | allow network for this worker |
| `sandbox` | `none`, `landlock` or `container`. A profile can **raise** a worker's isolation above the session's, never lower it |
| `max_steps`, `max_tokens` | a lifetime cap for this worker |
| `mem`, `cpus`, `pids`, `image`, `ports` | for containers, see [chapter 5](05-containers.md) |

A worker with a cap stops when it reaches it, and tells the lead; waking it with a message does **not** reset the cap:

```text
$ zcode run --auto --max-workers 2 "run a worker"      # profile "cheap": {"max_steps": 2}
▸ spawn loop
▸ wait
[from w1] [stopped: worker step cap reached]
worker stopped
```

## Team settings

All optional, under `"swarm"` in the config:

| Key | Default | Meaning |
|---|---|---|
| `max_workers` | 0 (off) | how many workers the lead may start (same as `--max-workers`) |
| `max_depth` | 1 | 1 means workers cannot start workers |
| `pair_cap` / `total_cap` / `chain_cap` | 40 / 400 / 16 | the message limits above |
| `concurrency` | 4 | model requests in flight at once |

Next: [containers](05-containers.md).
