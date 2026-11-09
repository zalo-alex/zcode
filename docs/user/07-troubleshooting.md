# 7. When something goes wrong

Messages are quoted exactly as zcode prints them. Find yours, then read what to do.

## Setting up

| Message | What it means and what to do |
|---|---|
| `` error: no model configured: set `model` in config, --model provider/model, or ZCODE_MODEL ``  | zcode does not know which model to use. Put `"model": "provider/model"` in `~/.config/zcode/config.json`, or pass `-m provider/model` ([chapter 1](01-install-and-first-run.md#connect-a-model)) |
| `` error: provider for model `demo/m` not found (configured providers: none) ``  | the model names a provider that is not in your config. `ZCODE_BASE_URL` and `ZCODE_API_KEY` only adjust a provider that already exists in the config, they do not create one |
| `error: LLM HTTP 401: {"error":{"message":"invalid api key"}}` | the server rejected your key. Check `apiKey` (or `ZCODE_API_KEY`) and the `baseURL` |
| `error: invalid value 'banana' for '--sandbox <SANDBOX>'` | a typo in a flag. The message lists the allowed values; exit code 2 |

## The sandbox says no

| What you see | What it means and what to do |
|---|---|
| `cat: …: Permission denied`, `cannot create …: Permission denied` in a command's output | the sandbox blocked it, which is its job. If the access is legitimate, allow exactly that: `--allow-read DIR`, `--allow-write DIR` |
| `Operation not permitted` when a command opens a network connection, or a download or `git clone` fails to resolve or connect | no network by default. Add `--net` |
| a tool like `cargo`, `npm` or `pip` cannot write its cache | `~/.cargo`, `~/.cache`, `~/.npm` are read-only. Use `--allow-write ~/.cache` (or point the cache into the project) |
| `` error: tier 1 sandbox (Landlock + seccomp) cannot be enforced: /home/you contains /home/you/.config/zcode which must stay inaccessible; run from a project subdirectory or use `--sandbox none`. ``  | you started zcode in a folder that *contains* your secrets, usually your home folder. `cd` into the project |
| `error: tier 1 sandbox (Landlock + seccomp) cannot be enforced: Landlock is not available on this kernel …` | the kernel is too old or Landlock is not enabled (`cat /sys/kernel/security/lsm` should list `landlock`). Options: `--sandbox container`, `--sandbox warn` (runs without protection, with a warning), `--sandbox none`. With `--auto` zcode will not run unprotected on its own |
| `error: --require-sandbox: sandbox is disabled (--sandbox none)` | you asked for a guarantee and for no sandbox at once |
| `Error: user denied this action` | zcode asked for permission and nobody could answer (no terminal) or you said no. Run on a terminal, or use `--auto` |
| `Error: blocked: untrusted file/web content contained instructions; mutating actions are disabled for the rest of this turn.` | the [injection guard](03-safety-and-sandbox.md#the-injection-guard) saw a file or page trying to give orders. Review that content; `--guard warn` only warns |
| `Error: file exists; read it first before overwriting` | an agent tried to overwrite a file it had not read. It will read it and retry |
| `modified by another agent, re-read` | two agents touched the same file; the second must re-read it first. Give each worker its own files via `write` |

## Teams (workers)

| Message | What to do |
|---|---|
| `` Error: agent `w1` already exists ``  | worker names must be unique within a run |
| `Error: worker limit reached (2)` | raise `--max-workers` |
| `` Error: unknown profile `x` ``  | the profile is not defined under `"profiles"` in your config |
| `` Error: invalid agent name `x` (letters, digits, _ and -; not `lead`) ``  | pick a simpler name |
| `` Error: no such agent `x` (agents: lead, docs, tests) ``  | `send` went to a name that does not exist; the list shows the real ones |
| `Error: hop limit reached …` | agents were replying to each other in circles and were cut off, on purpose. Give them a clearer split of work |
| `[stopped: worker step cap reached]` | a profile's `max_steps` / `max_tokens` cap on that worker was hit |
| `` Error: profile `lax` asks for tier 0 below the session tier 2; a profile may only raise the tier ``  | a worker cannot be less isolated than the session |
| the run ends with exit code 3 | a limit was reached; for a team the limit covers all agents together. Raise `--max-steps`, `--max-tokens` or `--max-time` |

## Containers

| Message | What to do |
|---|---|
| `` tier 2 sandbox cannot be enforced: image `nope` not found in …; import one with `zcode image import <tar\|dir> --name nope` `` | no image by that name. `zcode image ls` shows what you have; `zcode image pull alpine:3.20` gets one |
| `` tier 2 sandbox cannot be enforced: OCI runtime `crun` not found in PATH; install crun … ``  | install `crun` |
| `user namespaces are disabled (user.max_user_namespaces=0); enable them with sysctl` | `sudo sysctl user.max_user_namespaces=15000` (make it permanent in `/etc/sysctl.d/`) |
| `` unprivileged user namespaces are restricted by AppArmor; run `sysctl kernel.apparmor_restrict_unprivileged_userns=0` … ``  | Ubuntu 24.04 and newer: relax that setting, or add an AppArmor profile for zcode |
| `` overlayfs cannot be mounted inside a user namespace (needs Linux 5.11+) and `fuse-overlayfs` is not installed … ``  | install `fuse-overlayfs`, or set `"container": {"rootfs": "copy"}` (works, but slow to start for big images) |
| `` net=true needs `pasta` (passt) or `slirp4netns` (slirp4netns) to attach the container to the network; install one, or use net=false ``  | install `passt` (it provides `pasta`) or `slirp4netns`, or run the agent without network |
| `--require-limits: cgroup v2 delegation is not available for this user …` | memory/CPU limits need cgroup v2 delegation. Without `--require-limits` zcode just skips them with a warning |
| `` error: image `alpine` already exists; remove it first with `zcode image rm alpine` ``  | pull or import under another `--name`, or remove the old one |
| `error: https://registry-1.docker.io/v2/…: not found (check the image name and tag)` | the image or tag does not exist |
| `error: the registry requires credentials (only anonymous bearer-token pulls are supported)` or `… access denied (private images need credentials, which are not supported)` | private registries are not supported. Pull it elsewhere and `zcode image import` the tarball |
| a worker cannot reach a server another worker started, or the lead cannot connect to the forwarded port | each container has its own network. A tier 1 lead needs `--net` to reach a forwarded port; otherwise test inside the worker ([chapter 5](05-containers.md#network-and-ports)) |

If a container run dies halfway, nothing is left behind: containers end with zcode, and the next run removes leftovers.

## Still stuck?

- Re-run with `--format json`: every tool call, its input and its output are in the events.
- `zcode sessions` and `--continue` let you pick the conversation back up.
- Every flag and setting is in the [technical reference](../technical/PROTOCOL.md); known limits are in [KNOWN_GAPS.md](../technical/KNOWN_GAPS.md).
