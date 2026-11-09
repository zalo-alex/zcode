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

- Only `run --auto …` is handed over, because a daemon has no terminal to ask you questions on. The interactive mode, `--continue` and `--session` always run on their own (the interactive mode only goes through the daemon when you ask for a group with `--group`, see above).
- If no daemon answers, or its socket is stale, zcode quietly runs on its own: nothing changes for you.
- Your model settings, folder, sandbox level and limits come **from your command**, never from the daemon's environment, and your API key stays inside the daemon. A daemon started by one user cannot be used by another.

## Two terminals, two agents, talking to each other

With a daemon running, each terminal can run its own interactive agent and put it in a **group**. Agents in the same group can message each other directly. There is no lead: each of them answers to the person at its own terminal, and they talk when they need to.

Start the daemon once, then join from as many terminals as you like. Give each agent a name so the others can address it:

```text
# terminal 1
$ zcode --group duo --agent alice --auto

# terminal 2
$ zcode --group duo --agent bob --auto
```

Here is what the two screens look like after alice's user asks her to get bob working on a failing test. Lines starting with `›` are what the person typed; nobody typed anything in bob's terminal.

```text
alice's terminal
----------------
zcode 0.3.0 · my-model · ~/shop
joined group `duo` as `alice` · auto-approve · sandbox landlock
in the group: alice
your agent can message the others with send; what they send back shows up here as it arrives
/help  /agents  /cost  /status  /exit   (Ctrl-C interrupts your agent; anything else you type is sent to it)
› tell bob the cart test is failing
▸ send bob: The cart tests are failing in cart.py (discount). Can you look at test_cart.py?
I asked bob to look at the failing test.
‹ bob On it: I will fix test_cart.py and tell you when it passes.
Bob says he is on it and will edit test_cart.py.
› /agents
alice      tier 1 idle     queued 0 steps 3 tokens 4500/120
bob        tier 1 idle     queued 0 steps 2 tokens 2700/80
swarm budget: 5 steps, 7200 prompt tokens
```

```text
bob's terminal
--------------
zcode 0.3.0 · my-model · ~/shop
joined group `duo` as `bob` · auto-approve · sandbox landlock
in the group: alice, bob
your agent can message the others with send; what they send back shows up here as it arrives
/help  /agents  /cost  /status  /exit   (Ctrl-C interrupts your agent; anything else you type is sent to it)
‹ alice The cart tests are failing in cart.py (discount). Can you look at test_cart.py?
▸ send alice: On it: I will fix test_cart.py and tell you when it passes.
I told alice I am fixing test_cart.py.
```

What to notice:

- `▸ send bob: …` is alice's agent using its `send` tool. The message crossed to bob's terminal as `‹ alice …`.
- Bob's agent woke up **by itself**, read the message, worked on it and replied. Bob's person only had to be looking at the screen, and can type to it at any moment.
- Alice's agent was idle, got bob's reply, woke up and told her user about it (`‹ bob …`, then her own summary).

### How it works

| | |
|---|---|
| **The two agents are not in your terminals.** | They live in the daemon. Each terminal is only a window onto one agent: it shows what the agent does and sends what you type. |
| **Everything is auto-approved.** | The daemon cannot ask you questions, so `--group` needs `--auto`. Commands still run in the [sandbox](03-safety-and-sandbox.md) (or a [container](05-containers.md): `--sandbox container`). |
| **Each agent has its own folder and settings.** | Start both in the same project folder when they should share files. Messages carry summaries and paths, not file contents, so agents in different folders cannot see each other's files. |
| **Typing while the agent is busy is fine.** | Your line is queued (`(your agent is busy; sent when it is done)`) and sent when the agent finishes, including when a colleague's message made it start working. |
| **Messages from a colleague are labelled.** | An agent treats them as a request from a colleague, not as an order: only its own user can change its task. |
| **No runaway chatter.** | Agents that answer each other without a person in between are cut off after a while (16 replies in a chain, 40 messages between a pair, 400 in all). Whenever a person types in a terminal, the counters start over. |
| **No budget for the whole group.** | A group is meant to stay open all day. `--max-steps`, `--max-tokens` and `--max-time` still apply to each turn. |

### In the group terminal

| Command | What it does |
|---|---|
| anything else | goes to your agent |
| `/agents` | who is in the group, whether each is working or idle, and their token use |
| `/cost` | tokens your agent has used |
| `/status` | group, name and whether it is working |
| Ctrl-C | interrupts what your agent is doing right now |
| `/exit` (or Ctrl-D) | leave the group: your agent is closed (its conversation stays on disk) |

If a terminal is closed or killed, its agent leaves with it, so nothing keeps answering your colleagues with nobody watching.

### When joining fails

```text
$ zcode --group duo --agent alice --auto
error: no zcode daemon is running at /run/user/1000/zcode/zcode.sock; start one with `zcode serve`

$ zcode --group duo --agent alice
error: --group runs your agent inside the daemon, which cannot ask for permission: add --auto (commands still run in the sandbox)

$ zcode --group duo --agent alice --auto
error: instance `alice` already exists; pick another name with --agent
```

`--group` is for the interactive mode only; `zcode run --group …` is refused.

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

Other programs talk to the daemon over the socket with a small length-prefixed JSON protocol (create an agent, send it a prompt, attach to its event stream, cancel, close). Agents survive their client disconnecting and can be re-attached by name; agents in the same *group* can message each other, and agents in different groups never can. People get the same thing from the command line: see *Two terminals, two agents* above. The protocol is documented in the [technical docs](../technical/DAEMON.md).

One thing to know: if a bug makes an agent panic, the daemon only survives it when it was built with `cargo build --profile serve`. The normal release binary prefers to be smaller, and a panic there stops the daemon.

Next: [when something goes wrong](07-troubleshooting.md).
