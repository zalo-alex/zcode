# zcode user guide

zcode is a coding agent for your terminal. You tell it what you want in plain words; it reads your files, edits them and runs commands to get there. This guide shows how to use it, with real commands and what they print.

## Read in this order

1. [Install and first run](01-install-and-first-run.md): install, connect a model, your first requests
2. [Everyday use](02-everyday-use.md): fixing code, sessions, scripts and pipes, limits, JSON output
3. [Safety and the sandbox](03-safety-and-sandbox.md): what zcode can and cannot touch, and how to widen it
4. [Teams of agents](04-teams-of-agents.md): let zcode split a job between workers
5. [Containers](05-containers.md): give every agent its own private machine
6. [The background service](06-daemon.md): `zcode serve`
7. [When something goes wrong](07-troubleshooting.md): messages and what to do about them

Looking for every flag and config key? See the [technical docs](../technical/README.md).

## A two-minute tour

```text
$ zcode run "what does this project do?"
▸ ls
▸ grep .
▸ read cart.py
cart.py has one function, `total(items, discount=0)`: it sums price * qty and subtracts `discount`.
```

```text
$ zcode run --auto "the discount in cart.py is wrong, fix it and run the test"
▸ read cart.py
▸ edit cart.py
edited /home/you/shop/cart.py (1 replacement)
@@ -2,3 +2,3 @@
     """Sum of price * qty, minus a percentage discount."""
     subtotal = sum(i["price"] * i["qty"] for i in items)
-    return subtotal - discount
+    return subtotal * (1 - discount / 100)
▸ bash python3 test_cart.py
Fixed: `discount` is a percentage, but `total()` subtracted it as an amount. It now returns `subtotal * (1 - discount / 100)` and test_cart.py passes.
```

Every `▸` line is a tool zcode used. Nothing outside your project is touched unless you allow it.

> The model's wording in these examples is made up; everything else (the `▸` lines, diffs, errors, `/agents` tables, exit codes) is the real output of zcode 0.3.0.

## Cheat sheet

| I want to… | Command |
|---|---|
| Ask one question and exit | `zcode run "explain cart.py"` |
| Chat interactively | `zcode` |
| Let it edit and run things without asking | `zcode run --auto "fix the failing test"` |
| Feed it text from a pipe | `echo "list the files" \| zcode run --auto` |
| Carry on where I left off | `zcode --continue` or `zcode run --continue "and now?"` |
| Name a conversation | `zcode run -s billing "start the refactor"` |
| List past conversations | `zcode sessions` |
| Machine-readable output | `zcode run --format json "…"` |
| Stop after N model steps | `zcode run --max-steps 20 "…"` |
| Let commands use the network | `zcode run --net "…"` |
| Let commands write somewhere else | `zcode run --allow-write ~/out "…"` |
| Let commands read somewhere else | `zcode run --allow-read ~/data "…"` |
| Turn the sandbox off | `zcode run --sandbox none "…"` |
| Refuse to run unless sandboxed | `zcode run --require-sandbox "…"` |
| Use several workers | `zcode run --auto --max-workers 3 "…"` |
| Run in a private container | `zcode run --auto --sandbox container "…"` |
| Download a container image | `zcode image pull alpine:3.20` |
| Let agents in two terminals talk to each other | `zcode serve`, then `zcode --group duo --agent alice --auto` and `zcode --group duo --agent bob --auto` |
| Update zcode | `zcode upgrade` |

## Exit codes

| Code | Meaning |
|---|---|
| 0 | finished |
| 1 | an error (model, config, sandbox that cannot be enforced, …) |
| 2 | a command-line mistake |
| 3 | stopped by a limit (time, steps, tokens) |
| 130 | interrupted with Ctrl-C |
| 143 | terminated |
