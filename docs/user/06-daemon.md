# 6. The background service (`zcode serve`)

Normally every `zcode` command is its own small process that starts, works and exits. That is all most people need.

`zcode serve` is an **optional** background service for people who run many agents, or who build a tool on top of zcode. One long-running process then hosts all the agents, shares one pool of connections to your model, and enforces one set of limits across them. Nothing starts it for you.

## Start it

In a terminal (or under your process manager of choice):

```text
$ zcode serve
zcode daemon listening on /run/user/1000/zcode/zcode.sock
```

It stays in the foreground. Stop it with Ctrl-C (or `kill`): it closes every agent it hosts and removes its socket.

The socket is private to you: the folder is mode `0700`, the socket `0600`, and zcode also checks that every client runs as your user.

```text
$ ls -l $XDG_RUNTIME_DIR/zcode/zcode.sock
srw------- 1 you you 0 Nov  9 11:37 /run/user/1000/zcode/zcode.sock
```

## Use it from the command line

You do not have to change anything. While a daemon is running, `zcode run --auto "…"` notices the socket, hands the job to the daemon, shows the result as usual and exits with the same code:

```text
$ zcode run --auto "say hello"
▸ bash echo hello from the daemon
The command printed: hello from the daemon
```

The output is the same as when it runs on its own, which you can force with `ZCODE_NO_DAEMON=1`:

```text
$ ZCODE_NO_DAEMON=1 zcode run --auto "say hello"
▸ bash echo hello from the daemon
The command printed: hello from the daemon
```

Details:

- Only `run --auto …` is handed over, because a daemon has no terminal to ask you questions on. The interactive mode, `--continue` and `--session` always run on their own.
- If no daemon answers, or its socket is stale, zcode quietly runs on its own: nothing changes for you.
- Your model settings, folder, sandbox level and limits come **from your command**, never from the daemon's environment, and your API key stays inside the daemon. A daemon started by one user cannot be used by another.

## Settings

Under `"serve"` in the daemon's own config, all optional:

| Key | Default | Meaning |
|---|---|---|
| `max_instances` | 64 | how many agents it will host at once |
| `max_history_bytes` | 8 MiB | conversation size per agent before it is compacted |
| `concurrency` | 8 | model requests in flight at once, across everything |
| `max_steps`, `max_tokens` | unlimited | budgets for everything the daemon does |
| `apps` | none | `{"myapp": {"token": "…"}}`: require every client to log in as an app, and keep each app's agents apart |

## Building on it

Other programs talk to the daemon over the socket with a small length-prefixed JSON protocol (create an agent, send it a prompt, attach to its event stream, cancel, close). Agents survive their client disconnecting and can be re-attached by name; agents in the same *group* can message each other, and agents in different groups never can. The protocol is documented in the [technical docs](../technical/DAEMON.md).

One thing to know: if a bug makes an agent panic, the daemon only survives it when it was built with `cargo build --profile serve`. The normal release binary prefers to be smaller, and a panic there stops the daemon.

Next: [when something goes wrong](07-troubleshooting.md).
