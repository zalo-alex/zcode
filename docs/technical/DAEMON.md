# `zcode serve`: daemon and embedded instances (opt-in)

One resident process hosting many agent instances on the same in-memory bus. Instances in the same `group` are agents of one swarm (channels, not the socket); instances in different groups, or without a group, are fully isolated. Nothing starts a daemon automatically.

```
zcode serve          # foreground; Ctrl-C / SIGTERM closes every instance and removes the socket
```

Config (`serve` key of the daemon's own config): `max_instances` (64), `max_history_bytes` (8 MiB per instance; over that the context is compacted before a turn), `event_ring` (2000 events kept per instance), `concurrency` (8 model requests at once, daemon-wide), `max_steps` / `max_tokens` (daemon-wide budgets, unlimited by default), `apps` (`{"name": {"token": "..."}}`; when present every connection must `hello` with a valid app and token).

## What comes from where

* **Everything an instance needs comes from the client's `create` request**: `cwd`, a `config` object (the same schema as a config file: `provider.<id>.options.{baseURL,apiKey}`, `model`, optional `small_model`, `profiles`, `swarm`), `sandbox`, `policy`, `limits`. The daemon's own environment, config files and credentials are never used for an instance (`ZCODE_*`/`OPENCODE_*` model overrides are ignored; `bash` runs with a scrubbed environment even at tier 0).
* The API key lives only inside the daemon. It is never sent to clients (events do not contain it), and never enters a container.
* Model clients (HTTP connection pools) are shared across instances that use the same base URL, key and model; one semaphore bounds model requests daemon-wide and one budget bounds total steps/tokens.

## Socket and framing

`$XDG_RUNTIME_DIR/zcode/zcode.sock` (fallback `/tmp/zcode-<uid>/zcode.sock`), directory 0700, socket 0600, `SO_PEERCRED` checked (same uid only). Frames are the same as the guest protocol: 4-byte big-endian length + a JSON object, at most 8 MiB. Requests carry `req` (a client-chosen id) and `op`; every request gets exactly one reply `{"req":N,"ok":true,...}` or `{"req":N,"ok":false,"err":"..."}`. Events are separate frames without `req`.

## Operations

| op | fields | notes |
|---|---|---|
| `hello` | `app`, `token` | only needed when `serve.apps` is set; instances and groups are namespaced per app |
| `create` | `instance`, `cwd`, `config`, `model?`, `sandbox?` (default `landlock`), `profile?`, `policy?` `{read,write,net,outside}`, `limits?` `{max_time,max_steps,max_tokens}`, `group?`, `max_workers?`, `stream?`, `session?` | allocation only: no model client, sandbox, session file or agent exists until the first `prompt`/`send`/`spawn`. Instance and group names are `[A-Za-z0-9_-]{1,32}` |
| `attach` | `instance`, `since?` | subscribes this connection to the instance's events; the ring is replayed first (events with `seq > since`) |
| `detach` | `instance` | |
| `prompt` | `instance`, `text` | one turn at a time (`instance is busy` otherwise); the end of the turn arrives as a `turn_end` event |
| `send` | `instance`, `to`, `text` | message an agent of the instance's swarm (a worker or, in a group, another instance); an idle instance wakes up and answers |
| `spawn` | `instance`, `name`, `prompt`, `write?`, `profile?` | same semantics as the tool |
| `cancel` | `instance` | interrupts the running turn (`turn_end` with `stop: "interrupted"`) |
| `close` | `instance` | stops workers, closes sandboxes/containers, forgets the instance |
| `status` | `instance?` | state, queue, per-agent budgets (as `/agents`) |

## Events

The `--format json` events (`step_start`, `text`, `tool_use`, `step_finish`, `error`, `reasoning`) plus the swarm events (`spawn`, `message`, `idle`, `port`), each with `instance` and `agent` (`agent` is the instance name for its lead) and a per-instance `seq`. Extra events: `turn_end` `{stop: done|limit|interrupted|error, code: 0|3|130|1, message, text}` (the exit codes of `zcode run`), `warning`, `closed`; with `stream: true` also `text_delta`, `reasoning_delta` and `tool_start`.

## Lifetime and limits

* Instances outlive connections and are re-attached by id; a dead client never takes its instances down.
* `max_instances` caps the number of instances; history is compacted over `max_history_bytes`; tool output is capped (30 KB per call, the rest spilled to a file); `event_ring` bounds the replay buffer.
* A panic inside one instance's turn is caught at the task boundary: that instance reports an `error` + `turn_end` and refuses further prompts, the daemon and other instances keep running. This needs `panic = "unwind"`: build the daemon with `cargo build --profile serve` (release plus unwinding; +~0.5 MB binary, +~0.3 MB idle RSS). The default release profile keeps `panic = "abort"` to protect the startup/RSS budget, and `zcode serve` prints a note when it was built that way: there a panic stops the whole daemon.

## `zcode run` as a thin client

`zcode run --auto ...` connects to the daemon when its socket answers: it sends its own resolved config (its env and files already applied) in `create`, attaches, prompts, renders the event stream and exits with the turn's code; `--format json` output is the embedded stream (without the daemon-only `instance`/`seq` fields). With no daemon, a stale socket, `ZCODE_NO_DAEMON=1`, `--continue`/`--session`, or without `--auto` (a daemon cannot prompt your terminal), it falls back to the embedded runtime. Human output is rendered from streamed deltas, so it matches the embedded output except for the colored edit diffs.
