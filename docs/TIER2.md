# Tier 2: one rootless container per agent

`--sandbox container` (or a profile with `"sandbox": "container"`) runs **all tools of an agent inside its own rootless OCI container**: own root filesystem, own PID/IPC/UTS/mount/net namespaces. `webfetch` stays host-side, gated by the agent's `net` policy. Nothing from the host is visible except the project directory.

## Requirements (checked lazily at the first tier 2 start, cached; fail closed)

| Need | Why | Missing → |
|---|---|---|
| an OCI runtime (`crun` default; any runtime that accepts a bundle) | `create/start/state/kill/delete` | error naming the runtime and how to install it |
| unprivileged user namespaces | rootless | error naming the sysctl / AppArmor knob |
| overlayfs inside a user namespace (Linux 5.11+) or `fuse-overlayfs`, else `container.rootfs: "copy"` | private root fs | error |
| `pasta` (passt) or `slirp4netns`, only for `net: true` / `ports` | outbound network + port forwarding | error (never a silent downgrade) |
| cgroup v2 delegation, only for `mem/cpus/pids` | limits | warn once and skip; `--require-limits` fails |
| a static musl `zcode-guest` embedded by `build.rs` | container init + tool server | error |

In a swarm, tier 2 prerequisites are checked at the first tier 2 `spawn`; a failure is a tool error and the lead keeps running.

## Config

```json
"sandbox": "container",
"container": { "runtime": "crun", "image": "alpine", "idmap": "single",
               "rootfs": "auto", "seccomp": "default", "readonly_rootfs": false,
               "port_range": [20000, 29999], "net_backend": "auto", "pasta": "pasta", "slirp4netns": "slirp4netns",
               "runtime_args": ["--cgroup-manager=disabled"] },
"profiles": {
  "build": { "sandbox": "container", "net": true, "mem": "2g", "cpus": 2, "pids": 512 },
  "web":   { "sandbox": "container", "net": true, "ports": [3000], "image": "node-20" }
}
```

* `idmap: "single"` (default): the host uid is mapped to container root, no `newuidmap`/subuid needed, and files agents create in the project belong to the host user (required for mixed tiers). `"full"` is accepted by the config parser but **not implemented** (refused with an actionable error).
* `rootfs`: `auto` picks kernel overlayfs, then `fuse-overlayfs`, then a reflink/copy of the base (see `bench/BENCH.md` for the prototype numbers).
* `net_backend`: `auto` (default) uses `pasta` and falls back to `slirp4netns` when `pasta` is not installed; `pasta` or `slirp4netns` force one and fail closed when it is missing. With `slirp4netns` the container resolves names through its built-in resolver (`10.0.2.3`), forwarded ports listen on `127.0.0.1` only, and a server inside the container must bind `0.0.0.0` (not `127.0.0.1`) to be reachable through a forward.
* Flags: `--keep-state` keeps the upper layers after the run, `--require-limits`.

## Images

```
zcode image import <tar|tar.gz|tar.xz|tar.zst|dir> --name <n>   # in-process tar reader; refuses path traversal, never writes through symlinks, skips device nodes with a warning
zcode image ls | rm <n>
zcode image pull <ref> [--name n] [--platform os/arch[/variant]]  # in-process registry client (cargo feature `oci`); without it: skopeo + umoci
zcode image gc                                                    # (feature `oci`) delete cached layers no pulled image uses
```
Images live in `~/.cache/zcode/images/<name>/rootfs`.

**In-process pull** (`cargo build --features oci`): registry v2 API with anonymous bearer-token auth (tested against Docker Hub and a local fake registry; other registries that follow the same token flow should work; private images need credentials, which are not supported), multi-arch index with platform selection (default: this machine's `linux/<arch>`, override with `--platform`), sha256 verification of every manifest and layer (a mismatch is refused and never enters the cache), a content-addressed blob cache in `~/.cache/zcode/blobs/sha256/` (a repeat pull downloads nothing), up to 3 layers downloaded in parallel, and layers applied in order with whiteouts (`.wh.<name>`, `.wh..wh..opq`). Path traversal and writes through symlinks are refused, device nodes are skipped with a warning, and only `127.0.0.1`/`localhost` registries may use plain http. Pulled images carry an `image.json` (reference, manifest digest, layer digests) that `image gc` uses. gzip layers are decoded in-process; zstd, xz and bzip2 layers use the system decompressor. Docker schema 1 manifests and foreign layers are not supported.

## Layout of a container

* Root: overlayfs, lower = the image (read-only, shared), upper = `~/.local/state/zcode/<run-id>/<agent>/upper` (private). Installs and edits to `/` are invisible to every other agent.
* The project directory is bind-mounted at the **same absolute path**, read-only (`rbind,ro,rro`), then one read-write bind per assigned write path (OCI applies mounts in order). The lead gets the whole project read-write. `.git` is therefore read-only for workers unless explicitly granted. cwd = the project path.
* `/tmp` is a private tmpfs; `/dev` is minimal (null, zero, full, random, urandom, pts, shm); `/proc` is the container's own.
* Env is fixed: `PATH`, `HOME=/root`, `TERM`, `LANG`, `TMPDIR=/tmp`. No API key or host variable ever enters a container; the model client stays on the host.
* Hardening (OCI config, not the tier 1 code): own namespaces, `noNewPrivileges`, reduced capability set (the guest drops `CAP_NET_ADMIN` after bringing up `lo`), OCI default masked/read-only paths, per-agent hostname, `rootfsPropagation: private`, a default-allow seccomp profile denying `keyctl bpf mount unshare setns io_uring_* ptrace ...` (disable with `container.seccomp: "none"`), rlimits (`nofile`, `core`), optional `readonlyRootfs`.
* Network: `net: false` = empty netns with only `lo` (the guest brings it up). `net: true` = outbound through `pasta` attached to the container netns. `ports: [3000]` forwards an auto-allocated host port (from `port_range`) to the guest port and emits `{"type":"port","agent":..,"guest":3000,"host":N}`; `/agents` lists them. Every agent has its own netns, so every agent can bind the same port. A tier 1 lead with `net=false` cannot reach a forwarded port (AF_INET is denied): test servers inside the worker, or run the lead with `--net`.

## Processes and lifecycle

```
zcode ── zcode __ns <spec>   (single-threaded re-exec: unshare(user+mount), uid 0 -> host uid, mount overlay, then
   │                          runtime create / start; on stdin EOF: kill + delete)
   │         └─ container: /zcode-guest (pid 1 reaper) ── worker ── connects OUT to /run/zcode/g.sock
   └─ pasta (net=true)
```
* The container is created eagerly in the background at `spawn`, overlapping the agent's first model request; the first tool call awaits a ready future.
* The container dies with zcode: the supervisor sees its stdin close, and the guest exits when its socket closes.
* Each run holds a `flock`ed lock file under `~/.local/state/zcode/<run-id>/`. The first tier 2 start of a later run reaps dead runs' containers, `pasta` processes, state and socket dirs.
* Close = kill, delete, remove the upper layer unless `--keep-state`.

## Guest protocol

Per-agent unix socket in a private dir (`$XDG_RUNTIME_DIR/zcode/<run-id>/<agent>/`, mode 0700, socket 0600); that dir alone is bind-mounted into that agent's container at `/run/zcode`; the guest connects out and the host checks `SO_PEERCRED`. No agent can reach another's socket.

Frames: 4-byte big-endian length + JSON object, at most 8 MiB (`crates/zcode-tools/src/frames.rs`). Larger payloads are chunked.

```
→ {"id":N,"op":"exec|bash_output|bash_kill|read|write|edit|multiedit|glob|grep|ls|ping|shutdown","args":{...}}
← {"id":N,"ev":"out","data":"..."}            (zero or more chunks, each <= 1 MiB)
← {"id":N,"ev":"done","ok":true,"code":0,"timed_out":false,"cwd":"/proj"}      or  {"ok":false,"err":"..."}
```
The host treats guest replies as untrusted data: bounded frames and totals (64 MiB per reply), validated shape, unknown ids ignored, any violation drops the connection and fails pending calls. Huge outputs are truncated in the guest (head/tail) and spilled under `/run/zcode/spill`. Stale-edit detection runs inside the guest (a per-agent content-hash map), which works across the host/guest boundary because every agent sees the same project files.
