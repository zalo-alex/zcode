# Design notes and known gaps

What works differently from what you might expect, what is not implemented, and what the sandbox cannot do.

## Design notes
* **Per-worker caps** (`max_steps` / `max_tokens` in a profile) are lifetime caps: waking an idle worker with a message does not reset them.
* **Tier 1 setup is split**: the kernel probe and the deny-conflict check run when the session opens (fail closed, immediate error); opening the Landlock rule fds and compiling the seccomp filter wait for the first `bash` call. A failure there is a `bash` tool error (still fail closed).
* **A single file can be a write root** in tier 1 (a worker assigned `test_cart.py`): the path guard keeps the parent directory's fd and opens only that file name. An existing parent directory is no longer re-checked against the write roots when writing (the file open itself is confined).
* **`/etc/resolv.conf` is followed**: Landlock rules attach to inodes, so a symlink that leaves `/etc` needs its target as a read-only rule or DNS fails under `--net`.
* **Release profile is `opt-level = "s"`** (0.2.0 used 3). See the idle-RSS note below.
* **`image import` hardening**: a directory entry that lands on a symlink replaces the symlink, never following it (so a crafted tar cannot change the mode of a directory outside the image). A file entry over a lower-layer directory replaces it.
* **No queued-input mechanism existed**: `Agent::turn` took a single string and the REPL blocked on `readline`. Push delivery is new: each model step first drains the agent's inbox (`Agent::drain_inbox`).
* **`multiedit` bypassed the outside-cwd check** in `permission.rs`; it is now covered.
* **`Ctx.read_files` was a path set with no content stamp**; stale-edit detection is new (per-file content hash recorded at read, compared at write/edit/multiedit, in the host process for tiers 0/1 and inside the guest for tier 2).
* **`--auto` / `--allow-outside` and tier 1.** They used to lift the outside-cwd restriction entirely. Under the default tier 1 they now widen only the *file tools* (to `/` minus the deny list: config dir, `~/.ssh`, `/proc`, `/sys`, the rest of `$HOME`); they do **not** widen `bash` (Landlock): use `--allow-write PATH` or `--sandbox none`. The existing test that writes outside the cwd with `--auto` passes because it is a file-tool write.
* **`Sandbox` trait** gained `cwd()` and `tier()` (needed for the persistent shell cwd and `/agents`), plus defaulted `ready()`, `ports()`, `spill_dir()`. `exec` returns a stream, but tier 0/1 collect the output first (it is already spooled to a file to survive backgrounded children), so streaming is real only over the guest protocol.
* **Landlock via raw syscalls, not the `landlock` crate.** The child must be confined from `pre_exec` without allocating and with a per-spawn `/proc/<pid>` rule; the crate's API consumes the ruleset and allocates. The three syscalls are small; ABI v1..v6 are handled (fs, TCP net on ABI 4+, signal/abstract-socket scoping on ABI 6). `seccompiler` builds the seccomp filter.
* **`/proc/self` rule.** Landlock rules attach to inodes, so a `/proc/self` rule would cover only the first shell, not `cargo`'s `rustc` or `gcc`. The child gets its own `/proc/<pid>` plus a few global `/proc` files; `sandbox` config can opt into all of `/proc` (`Policy.proc`), which exposes same-uid processes' `environ`/`cmdline`. The zcode process itself is marked non-dumpable so its `/proc/<pid>/environ` is not readable.
* **Tier 2 supervisor re-exec.** `unshare(CLONE_NEWUSER)` is impossible in a multi-threaded process, so the user+mount namespace and the overlay mount live in a single-threaded `zcode __ns` re-exec that also drives the runtime CLI (`create`, `start`, `state`, `kill`, `delete`). The runtime then runs as uid 0 *inside that user namespace* (no nested userns), which is why `--cgroup-manager=disabled` is the default for crun (configurable via `container.runtime_args`).
* **`ContainerOpts`/`open_with`** carry per-worker container settings; `open` keeps its old signature.

## Not implemented / partial
* **`idmap: full`** (newuidmap/subuid): parsed, refused with an actionable error. Only `single` works. Consequently the "warn if `idmap: full` with a host-tier lead" rule exists in `spawn` but cannot trigger until `full` is implemented.
* **`zcode image pull` in Rust** is built behind the cargo feature `oci` (registry v2 client, anonymous bearer auth, index/platform selection, sha256 verification, blob cache, whiteouts, `image gc`; see `docs/technical/TIER2.md`). It is **off by default** (the release workflow does not enable it either): a default build still shells out to `skopeo` + `umoci`. Gaps: no credentials for private registries, no Docker schema 1 manifests or foreign layers, zstd/xz/bzip2 layers use the system decompressor (gzip is in-process), and it was exercised against Docker Hub and a local fake registry only. The feature costs +152 KB of binary and nothing at idle.
* **cgroup v2 limits** (`mem`, `cpus`, `pids`) are generated into `config.json` and used when the user has a delegated cgroup; I could not exercise the *enabled* path (WSL2 has no delegation), only the skip/`--require-limits` path.
* **slirp4netns fallback**: implemented (`container.net_backend`: `auto|pasta|slirp4netns`) and tested end to end (tap configured, built-in resolver, a forwarded port reached from the lead), but only with slirp4netns 1.2.1. Differences from pasta: forwards listen on 127.0.0.1 only and the guest server must bind 0.0.0.0.
* **runc / youki**: only crun was available and tested. The config uses no crun-only OCI features; the only crun-specific bit is the default `--cgroup-manager=disabled`, overridable.
* **Not built**: shared per-group netns, evicting idle containers in daemon mode (`keep upper layer, recreate lazily`), `--keep-state` is a flag but there is no `reattach`. Daemon instances are created lazily but their *containers* are not evicted when idle.
* **Interactive groups** (`zcode --group`): always auto-approve (the daemon cannot ask); agents learn who else is in the group from their user or from the `no such agent` error, there is no tool to list members; `/cost` shows only the terminal's own agent and there is no `/clear`, `/compact`, `/thinking` or `/todo` in group mode; agents in different folders share messages but not files; a group's message caps are re-armed by any person typing in any of its terminals.
* **Daemon groups**: workers spawned by one instance of a group use that instance's own client/policy/limits; swarm budgets and the session base of the first instance created in a group apply to the whole group.
* **Thin client**: human output is rendered from streamed deltas and omits the colored edit diffs; only `--auto` runs are delegated (a daemon cannot prompt a terminal).
* **32-bit / non-x86_64 seccomp compat** syscalls are not specially handled in the tier 1 denylist (the arch of the build is used).

## Inherent limitations
* Tiers 0, 1 and 2 share the host kernel.
* Landlock restricts access, not visibility: path existence is still observable (`stat` reports EACCES vs ENOENT).
* Same-UID signals are unscoped before kernel 6.12 (Landlock ABI 6) in tier 1; on 6.12+ they are scoped to the domain.
* Rootless cgroup limits need delegation; otherwise they are skipped with a warning (or an error with `--require-limits`).
* A tier 1 policy cannot carve holes: running zcode in a directory that *contains* `~/.ssh` or `~/.config/zcode` (e.g. `$HOME` itself) is refused for bash; use a subdirectory or `--sandbox none`.
* Tier 1 `net=false` cannot reach a tier 2 worker's forwarded port (AF_INET is denied): test inside the worker or give the lead `--net`.
* Daemon panic isolation requires `panic = "unwind"`: only the `serve` cargo profile has it; the default release profile keeps `panic = "abort"` (idle RSS and size), where a panic stops the whole daemon.

## Idle RSS (+0.35 MB over 0.2.0)
The first version of the sandbox work added ~0.55 MB (+9%) to the idle REPL. Measured per page (`/proc/<pid>/pagemap` + symbols, `bench/sandbox/textpages.py`): anonymous memory is identical to 0.2.0 (+20 KB); the growth is shared, file-backed code pages (`zcode_tools` 77 pages, `tokio` +32, `serde_json` +35, generics from both), not heap. Tier 1 setup is only ~6 of those pages; the embedded guest costs nothing (identical RSS built with and without it). Building with `opt-level = "s"` (no measurable slowdown: full-tree grep with and without literal prefixes, `bench/sandbox/toolcpu.py`) and making the tier 1 build lazy brought it to **~+0.35 MB (+6%)** and the binary from +3.2 MB to +0.5 MB. The remaining gap is code that runs at startup and is spread over more pages; it is page cache shared by every zcode process, but it is still above the 0.2.0 number.

## Rootfs mechanism chosen
Kernel overlayfs mounted inside the supervisor's user+mount namespace: constant ~10-20 ms regardless of image size (1.3 GB base measured), no helper process, upper layer = a plain directory per agent. `fuse-overlayfs` (similar mount time, userspace data path) is the automatic fallback; a reflink/copy of the base is the last resort (1.7-2.4 s for 1.3 GB on ext4). Numbers in `bench/BENCH.md`.
