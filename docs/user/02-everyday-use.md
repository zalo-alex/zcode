# 2. Everyday use

## Two ways to run it

| | Command | Use it for |
|---|---|---|
| Interactive | `zcode` | a back-and-forth conversation |
| One job | `zcode run "…"` | scripts, one-off questions, CI |

`zcode "…"` and `zcode -p "…"` are shortcuts for `zcode run "…"`.

## Fixing and building things

Describe the result you want. zcode looks, changes, and checks its own work:

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

Edits are shown as a diff so you can see exactly what changed. Mention how to check the result ("and run the test") and zcode will do it.

## Tools zcode can use

| Tool | What for |
|---|---|
| `read`, `ls`, `glob`, `grep` | look around: files, folders, patterns, text search |
| `write`, `edit`, `multiedit` | create files, change text in them (several changes at once for `multiedit`) |
| `bash`, `bash_output`, `bash_kill` | run commands, including long-running ones in the background (dev servers, watchers) |
| `webfetch` | download a web page (needs `--net`) |
| `task` | hand a broad "go and explore" job to a read-only helper to save room in the conversation |
| `todowrite`, `todoread` | keep a checklist for a long job (`/todo` shows it) |

## Pipes and scripts

If you give no message and stdin is a pipe, zcode reads the message from it:

```text
$ echo "list the files here" | zcode run --auto
▸ ls
README.md, cart.py, test_cart.py
```

Use `-C` to run in another folder without `cd`:

```text
$ zcode run -C ~/projects/api "which tests cover login?"
```

## Conversations (sessions)

Every run is saved as a session, so nothing is lost. Name one with `-s` to come back to it by name:

```text
$ zcode run -s cart-work "hello"
Hi! Ask me about the cart.

$ zcode run -s cart-work "do you remember what we said?"
Yes, I remember: we only said hello.
```

List what you have:

```text
$ zcode sessions
cart-work      4 msgs      0s ago  my-model  /home/you/shop
ses_6af1a09c0f2bf8      7 msgs      1s ago  my-model  /home/you/shop
ses_6af1a09b4cd14d      4 msgs      1s ago  my-model  /home/you/shop
```

Or just pick up the latest conversation **of the folder you are in**:

```text
$ zcode run "what is in cart.py?"
The cart has one function: total().

$ zcode run --continue "remind me what it is called"
As I said, it is total(items, discount=0).
```

`zcode --continue` (or `-c`) with no message opens the interactive mode on that conversation, and shows the last reply first.

## Machine-readable output

Add `--format json` and zcode prints one JSON event per line instead of text, which is easy to parse in scripts or other tools:

```text
$ zcode run --auto --format json "say hello"
{"agent":"lead","part":{"type":"step-start"},"sessionID":"ses_6af1a09b4cd14d","timestamp":1794220187913,"type":"step_start"}
{"agent":"lead","part":{"callID":"c0","state":{"input":{"command":"echo hello"},"output":"hello\n","status":"completed"},"tool":"bash","type":"tool"},"sessionID":"ses_6af1a09b4cd14d","timestamp":1794220187934,"type":"tool_use"}
{"agent":"lead","part":{"reason":"tool-calls","tokens":{"input":1200,"output":40},"type":"step-finish"},"sessionID":"ses_6af1a09b4cd14d","timestamp":1794220187934,"type":"step_finish"}
{"agent":"lead","part":{"type":"step-start"},"sessionID":"ses_6af1a09b4cd14d","timestamp":1794220187935,"type":"step_start"}
{"agent":"lead","part":{"text":"Printed hello.","type":"text"},"sessionID":"ses_6af1a09b4cd14d","timestamp":1794220187982,"type":"text"}
{"agent":"lead","part":{"reason":"stop","tokens":{"input":1500,"output":40},"type":"step-finish"},"sessionID":"ses_6af1a09b4cd14d","timestamp":1794220187983,"type":"step_finish"}
```

The `type` tells you what happened: `step_start` / `step_finish` (one model request, with token counts), `tool_use` (a tool ran, with its input and output), `text` (the model's words), and `error`. When you use [workers](04-teams-of-agents.md) there are also `spawn`, `message`, `idle` and `port` events, and every event says which `agent` it came from. The full list is in the [technical reference](../technical/PROTOCOL.md).

## Limits: stop a run that goes too long

A run stops by itself when it reaches a limit, and exits with code **3** so a script can tell:

```text
$ zcode run --auto --max-steps 3 "keep going"
▸ bash echo working
▸ bash echo working
▸ bash echo working
[stopped: step limit reached]
$ echo $?
3
```

| Flag | Limit | Default |
|---|---|---|
| `--max-steps N` | model requests per turn | 60 |
| `--max-tokens N` | prompt tokens used per turn | 400000 |
| `--max-time SECONDS` | wall-clock time per turn (`0` = none) | 1200 for `run`, none in the interactive mode |

zcode also stops by itself if it notices it is just reading files in circles ("exploration loop"). With [workers](04-teams-of-agents.md) these limits cover the **whole team**.

## Thinking and verbosity

The interactive mode shows the model's reasoning as it works; `zcode run` hides it. Change that with `--thinking` or `--no-thinking` (and `/thinking` in the interactive mode).

## Choosing the model

```text
$ zcode run -m myprovider/other-model "summarise cart.py"
```

`provider/model` must match what is in your [config](01-install-and-first-run.md#connect-a-model).

Next: [safety and the sandbox](03-safety-and-sandbox.md).
