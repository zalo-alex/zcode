# 1. Install and first run

## Install

```text
$ curl -fsSL https://raw.githubusercontent.com/zalo-alex/zcode/main/install.sh | bash
```

This puts one `zcode` binary on your machine (Linux, x86_64 or aarch64). Check it:

```text
$ zcode --version
zcode 0.3.0 (9d14ea7)
```

The part in brackets is the build; yours will differ. Update later with `zcode upgrade`.

Building from source instead? See the [technical docs](../technical/README.md#building-and-testing).

## Connect a model

zcode talks to any OpenAI-compatible chat endpoint. Tell it where in `~/.config/zcode/config.json`:

```json
{
  "model": "myprovider/my-model",
  "provider": {
    "myprovider": {
      "options": { "baseURL": "https://api.example.com/v1", "apiKey": "sk-..." }
    }
  }
}
```

- `model` is `provider/model-id`, where `provider` is a key under `provider`.
- You can keep the key out of the file: leave `apiKey` out and set `ZCODE_API_KEY` instead. `ZCODE_BASE_URL` overrides the URL the same way, and `ZCODE_MODEL` (or `-m provider/model`) overrides the model.
- A project can have its own config too: any later layer wins. The layers, lowest to highest, are `~/.config/opencode/opencode.json`, `~/.config/zcode/config.json`, the `OPENCODE_CONFIG_CONTENT` variable, and the `ZCODE_CONFIG_CONTENT` variable (JSON text). That last one is handy in scripts:

```text
$ ZCODE_CONFIG_CONTENT='{"model":"myprovider/my-model","provider":{"myprovider":{"options":{"baseURL":"http://localhost:8080/v1"}}}}' zcode run "hello"
```

If nothing is configured, zcode says what is missing:

```text
$ zcode run "hello"
error: no model configured: set `model` in config, --model provider/model, or ZCODE_MODEL
```

A wrong key is reported as the model's server said it:

```text
$ zcode run "hello"
error: LLM HTTP 401: {"error":{"message":"invalid api key"}}
```

Both exit with code 1.

## Your first request

`zcode run "…"` does one job and exits. Here it only reads, so it asks for no permission:

```text
$ zcode run "what does this project do?"
▸ ls
▸ grep .
▸ read cart.py
cart.py has one function, `total(items, discount=0)`: it sums price * qty and subtracts `discount`.
```

Each `▸` line is a tool call: `ls` listed the folder, `grep` searched, `read` opened a file. The last line is the answer.

## Letting it make changes

Tools that change things (`write`, `edit`, `bash`) ask first when you are at a terminal:

```text
$ zcode run "make a notes file"
▸ write notes.txt
? allow write: notes.txt [y/N/a(lways)]
```

Type `y` to allow this once, `n` (or Enter) to refuse, or `a` ("always") to allow that tool (`write`, `edit` or `bash`) for the rest of the session. If there is no terminal to ask (a script, a pipe) the action is refused, and the model is told so:

```text
Error: user denied this action
```

To approve everything up front, add `--auto` (or `-y`):

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

`--auto` does not mean unlimited: commands still run inside the [sandbox](03-safety-and-sandbox.md), which is on by default.

## Chatting: the interactive mode

Run `zcode` with no arguments for a conversation that keeps its context:

```text
$ zcode
zcode 0.3.0 · my-model · ~/shop
session ses_6af1a09eaf057d
/help  /clear  /compact  /thinking  /sessions  /status  /todo  /cost  /agents  /model  /exit   (Ctrl-C interrupts a turn)
› what is in cart.py?
▸ read cart.py
It is a 4-line module with one function, total().
› /status
session ses_6af1a09eaf057d · 4 msgs · ~465/128000 tokens · my-model · thinking on · cwd /home/you/shop
› /cost
tokens in 2700 / out 80 · ctx ~465/128000
› /exit
```

| Command | What it does |
|---|---|
| `/help` | list the commands |
| `/status` | session, size of the conversation, model, folder |
| `/cost` | tokens used so far (per agent when you use [workers](04-teams-of-agents.md)) |
| `/compact` | shrink the conversation to free up room |
| `/clear` | start the conversation over |
| `/thinking` | show or hide the model's reasoning |
| `/sessions` | list recent conversations |
| `/todo` | the task list the model keeps for itself |
| `/agents` | the workers of a [team](04-teams-of-agents.md) and their state |
| `/model` | show the model in use (change it with `-m provider/model` when you start) |
| `/exit` | quit (Ctrl-D works too) |

Press Ctrl-C to interrupt a turn without leaving.

## Giving zcode standing instructions

Put project rules in an `AGENTS.md` (or `CLAUDE.md`) file in your project: how to run the tests, code style, things to avoid. zcode reads the nearest one automatically. Personal rules go in `~/.config/zcode/AGENTS.md`.

Next: [everyday use](02-everyday-use.md).
