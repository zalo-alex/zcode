# 5. Containers

The default sandbox protects your machine from commands. A **container** goes further: the agent gets its own private computer-like environment, with its own root filesystem, its own processes and its own network. Whatever it installs or breaks stays inside.

Use a container when you:

- let several workers run builds or install packages without stepping on each other,
- run something you do not fully trust,
- need a specific environment (another distro, another set of tools).

It is *not* a virtual machine: a container shares your computer's kernel, like Docker does. And it needs no Docker: zcode drives a small runtime (`crun`) directly.

## What you need

| Need | Notes |
|---|---|
| `crun` | `apt install crun` (or `dnf install crun`). Any runtime that takes an OCI bundle, such as `runc` or `youki`, can be named in the config |
| Unprivileged user namespaces | on by default on most distros; some (Ubuntu 24.04+) restrict them with an AppArmor setting |
| Kernel 5.11+, or `fuse-overlayfs` | for the private root filesystem |
| `pasta` (`passt`) or `slirp4netns` | only if an agent needs network access or forwarded ports |
| cgroup v2 delegation | only for memory/CPU limits |

zcode checks these the first time you ask for a container and tells you exactly what is missing; it never silently falls back to something weaker (see [troubleshooting](07-troubleshooting.md)).

## Step 1: get an image

An image is a ready-made root filesystem. Download one:

```text
$ zcode image pull alpine:3.20
pulling alpine:3.20 (linux/amd64, 1 layers, 3.6 MB)
  layer 1/1 25f1d6b1951a done
pulled `alpine:3.20` as `alpine` into /home/you/.cache/zcode/images/alpine/rootfs
```

The name defaults to `alpine`; choose another with `--name`. Pulling again is nearly free because layers are cached and checked by their checksum:

```text
$ zcode image pull alpine:3.20 --name alpine-copy
pulling alpine:3.20 (linux/amd64, 1 layers, 3.6 MB)
  layer 1/1 25f1d6b1951a (cached)
pulled `alpine:3.20` as `alpine-copy` into /home/you/.cache/zcode/images/alpine-copy/rootfs
```

For a multi-architecture image, zcode picks your machine's. Ask for another one with `--platform`:

```text
$ zcode image pull busybox --platform linux/arm64 --name bb-arm
pulling busybox (linux/arm64, 1 layers, 1.9 MB)
  layer 1/1 55d2dadd4bbc done
pulled `busybox` as `bb-arm` into /home/you/.cache/zcode/images/bb-arm/rootfs
```

Mistakes are explained:

```text
$ zcode image pull alpine:no-such-tag --name nope
error: https://registry-1.docker.io/v2/library/alpine/manifests/no-such-tag: not found (check the image name and tag)

$ zcode image pull alpine:3.20
error: image `alpine` already exists; remove it first with `zcode image rm alpine`
```

Already have a root filesystem as a tarball or a folder? Import it:

```text
$ zcode image import ./rootfs --name tiny
imported `tiny` into /home/you/.cache/zcode/images/tiny/rootfs
```

(`.tar`, `.tar.gz`, `.tar.xz`, `.tar.zst` and plain folders work.) Manage what you have:

```text
$ zcode image ls
alpine  /home/you/.cache/zcode/images/alpine/rootfs
alpine-copy  /home/you/.cache/zcode/images/alpine-copy/rootfs
bb-arm  /home/you/.cache/zcode/images/bb-arm/rootfs
tiny  /home/you/.cache/zcode/images/tiny/rootfs

$ zcode image rm tiny
removed tiny

$ zcode image gc
removed 0 cached blob(s), freed 0.0 MB
```

`image gc` deletes downloaded layers that no image uses any more. It freed nothing above because `alpine` still uses them; after removing the images:

```text
$ zcode image rm alpine
removed alpine
$ zcode image rm bb-arm
removed bb-arm
$ zcode image rm alpine-copy
removed alpine-copy
$ zcode image gc
removed 2 cached blob(s), freed 5.5 MB
```

> Pulling needs a zcode built with the `oci` feature (`cargo build --release --features oci`). Without it, `image pull` uses `skopeo` and `umoci` if you have them, and `image gc` is not available. Private registries (logins) are not supported. `image import` always works.

## Step 2: run in a container

Name the image in your config, then use `--sandbox container`:

```json
{ "container": { "image": "alpine" } }
```

```text
$ zcode run --auto --sandbox container "look around"
▸ bash head -2 /etc/os-release; echo "user: $(id -un)"; echo "HOME=$HOME"; pwd; echo ---; ls /home/you
Inside the container:
NAME="Alpine Linux"
ID=alpine
user: root
HOME=/root
/home/you/shop
---
ls: /home/you: No such file or directory
README.md
cart.py
test_cart.py
project is writable
```

Read that carefully:

- It is Alpine Linux, even if your computer runs something else.
- It is "root" inside, but that root only exists inside the container; on your machine, files it creates still belong to **you**.
- Your project is there **at the same path** (`/home/you/shop`) and is writable.
- Nothing else of yours is: your home folder does not exist in there (`ls: /home/you: No such file or directory`), so no `~/.ssh` and no config with your API key. Your API key stays in zcode on the host and is never sent into a container.

## Every agent gets its own root filesystem

Each agent's root filesystem is a private layer on top of the shared, read-only image. Changes to `/` by one agent are invisible to the others:

```text
$ zcode --max-workers 2 --auto          # profile "box": {"sandbox": "container"}
› set up two workers
▸ spawn install tools
▸ wait
[from alice] alice sees: /alice.txt
▸ spawn look for alice's files
▸ wait
[from bob] bob sees: ls: /alice.txt: No such file or directory
alice changed her own root filesystem; bob could not see it.
```

`alice` created `/alice.txt`; `bob` (also in a container) cannot see it. Install a package, edit `/etc`, break something: it only affects that one agent, and everything is thrown away when zcode exits (keep it for debugging with `--keep-state`).

## The project folder is shared, with write ownership

- The **lead** (your main agent) can change the whole project.
- A **worker** sees the project read-only, except the paths you gave it with `write` when it was started. `.git` is read-only for workers unless you give it explicitly.

## Mixing levels: the lead on the host, workers in containers

Isolation is per agent. A profile can raise a worker's level above the session's, so a normal lead can use container workers for the risky parts:

```json
{
  "container": { "image": "alpine" },
  "profiles": { "box": { "sandbox": "container" } }
}
```

```text
$ zcode --max-workers 2 --auto
› build it
▸ spawn build
▸ wait
[from builder] Done: NAME="Alpine Linux"
builder finished.
› /agents
builder    tier 2 idle     queued 0 steps 2 tokens 2700/80 write /home/you/shop/test_cart.py
lead       tier 1 running  queued 0 steps 3 tokens 4500/120
swarm budget: 5 steps, 7200 prompt tokens
```

The **tier** column shows who is where: the lead is tier 1 (kernel sandbox), the builder tier 2 (container). The builder edited `test_cart.py` from inside its container and the lead can read and change that file normally, because it belongs to you.

A profile can never *lower* the level:

```text
$ zcode run --auto --max-workers 2 --sandbox container "go"        # profile "lax": {"sandbox": "none"}
▸ spawn x
spawn said:
Error: profile `lax` asks for tier 0 below the session tier 2; a profile may only raise the tier
```

The container is created in the background while the agent makes its first model request, so you do not wait for it. If a container cannot be started (no `crun`, say), that `spawn` fails with a clear error and the lead keeps working.

## Network and ports

A container has **no network** by default: only a loopback interface, so every agent can listen on port 3000 without clashing. Give it a profile with `net`, and optionally `ports` to reach its servers from your machine:

```json
{
  "container": { "image": "alpine" },
  "profiles": { "web": { "sandbox": "container", "net": true, "ports": [3000] } }
}
```

```text
$ zcode --max-workers 2 --auto
› start a server
▸ spawn serve
▸ wait
[from web] listening on 3000
server is up
› /agents
lead       tier 1 running  queued 0 steps 3 tokens 4500/120
web        tier 2 idle     queued 0 steps 2 tokens 2700/80 ports 26472->3000
swarm budget: 5 steps, 7200 prompt tokens
```

`26472->3000` means port 3000 inside the container is reachable at `127.0.0.1:26472` on your machine (picked automatically, from `port_range`; JSON mode prints a `port` event). Two workers can both serve on 3000 and get different outside ports.

Two things to know:

- A **tier 1 lead without `--net`** cannot connect to that forwarded port (the sandbox blocks network connections). Either test the server from inside the worker, or start the lead with `--net`.
- With `slirp4netns` instead of `pasta` (`"net_backend": "slirp4netns"`, or automatic when `pasta` is missing), the server inside must listen on `0.0.0.0`, not only `127.0.0.1`.

## Memory, CPU and process limits

```json
{ "profiles": { "build": { "sandbox": "container", "net": true, "mem": "2g", "cpus": 2, "pids": 512 } } }
```

These need cgroup v2 delegation (systemd gives it to your user session on most distros). Without it zcode skips them and warns once; add `--require-limits` to fail instead.

## Customize the container

Like `docker run -v/-e/--cap-add/--ulimit/--sysctl`. Put these under `"container"` in the config (all containers), or under `"container"` inside a profile (that agent only). A profile adds to the global lists, merges the maps by key and replaces single values.

```json
{
  "container": {
    "mounts": ["~/.cache/pip:/root/.cache/pip", "/data:/data:ro"],
    "tmpfs": ["/scratch:size=256m"],
    "env": { "FOO": "bar" },
    "cap_add": ["NET_RAW"],
    "ulimits": { "nofile": [8192, 8192] }
  },
  "profiles": { "build": { "sandbox": "container", "container": { "mounts": ["/opt/sdk:/opt/sdk:ro"] } } }
}
```

| Key | Meaning |
|---|---|
| `mounts` | host paths to bind: `"src:dst[:opts]"` (`ro`, `rw`, `suid`, `dev`) or `{"src","dst","ro"}`. `~` works; paths must be absolute; a missing source is an error. Default `nosuid,nodev`, read-write unless `ro`. |
| `tmpfs` | `"dst[:opts]"`, e.g. `/scratch:size=256m` |
| `env` | object or `["K=V"]`; overrides the defaults (`PATH`, `HOME`, ...). Host variables are never passed. |
| `hostname`, `user` | hostname; `"uid[:gid]"` inside the container |
| `cap_add`, `cap_drop` | capability names, with or without `CAP_` |
| `ulimits` | `{"nofile": 4096}` or `{"nofile": [soft, hard]}` |
| `sysctls` | `{"net.ipv4.ping_group_range": "0 0"}`. Rootless: only network-namespace sysctls work, and values must fit the single-id mapping (so `0 0`, not `0 2147483647`); `kernel.*` ones are refused with "Permission denied". |
| `devices` | host device nodes made available at the same path |
| `seccomp_deny`, `seccomp_allow` | add / remove syscalls in the deny list |
| `oci_patch` | any OCI `config.json` fields, merged last ([JSON merge-patch](https://www.rfc-editor.org/rfc/rfc7386): `null` deletes a key, arrays are replaced) |

You are in charge of what you mount: a mount can expose host files (`~/.ssh`) to the agent, and `oci_patch` can switch off protections such as `noNewPrivileges`. Mount read-only whenever you can.

## All container settings

Everything is optional, under `"container"` in the config:

| Key | Default | Meaning |
|---|---|---|
| `image` | `alpine` | the image to use (a profile can pick another with `image`) |
| `runtime` | `crun` | the OCI runtime program |
| `net_backend` | `auto` | `auto`, `pasta` or `slirp4netns` |
| `rootfs` | `auto` | how the private root is made: `auto`, `overlay`, `fuse`, or `copy` (slow for big images) |
| `readonly_rootfs` | `false` | make `/` read-only |
| `port_range` | `[20000, 29999]` | where forwarded ports are taken from |
| `seccomp` | on | `"none"` turns the container's syscall filter off |

## When it does not start

```text
$ zcode run --auto --sandbox container "hi"        # image "nope"
error: tier 2 sandbox cannot be enforced: image `nope` not found in /home/you/.cache/zcode/images; import one with `zcode image import <tar|dir> --name nope`

$ zcode run --auto --sandbox container "hi"        # runtime "no-such-runtime"
error: tier 2 sandbox cannot be enforced: OCI runtime `no-such-runtime` not found in PATH; install crun (or set container.runtime to runc/youki)
```

Containers die with zcode: if you kill it, they go too, and the next run cleans up anything left over.

Next: [the background service](06-daemon.md).
