# Benchmark note: before / after the sandbox work

Measured on WSL2 Debian 13 (kernel 6.6.87.2, 8 cores, ext4 vhd), release builds, rustc 1.99. "Before" is commit `a0597d1`
(zcode 0.2.0); "after" is this branch. Tier 2 used a 2 MB static-busybox rootfs. All numbers are from the scripts
described at the bottom; they are single-machine, small samples: read them as orders of magnitude, not as a spec.

## Regression check

| | before | after | note |
|---|---|---|---|
| `zcode --version` startup (400 runs, hyperfine) | 3.1 ± 0.8 ms | 2.9 ± 0.7 ms | no regression (re-measured on a noisier machine than the first run: 1.9 vs 2.0 ms then; the probes are lazy; user time 1.5 vs 1.4 ms) |
| idle REPL RSS (10 samples each) | 6.0-6.3 MB (mean 6.15) | 6.3-6.8 MB (mean 6.5) | **+~0.35 MB (~+6%)**, down from +0.55 MB: anonymous memory is identical (+20 KB), the rest is shared file-backed code pages. Fixed by `opt-level = "s"` and a lazy tier 1 build; the remainder is explained in `docs/technical/KNOWN_GAPS.md`. With `panic = "unwind"` (the `serve` profile) it is +~0.3 MB more |
| fixed prompt (system + tool schemas) | ~1475 tokens (14 tools) | ~1475 tokens (14 tools) | identical by default |
| fixed prompt with `--max-workers N` (lead) | n/a | ~1778 tokens (+3 tools, +~300) | opt-in only: the swarm tools and the swarm prompt are absent unless enabled |
| binary size | 7,956,936 B | 8,463,432 B (**+0.51 MB**; was +3.21 MB at `opt-level = 3`) | of which the embedded static musl guest is 2,475,040 B (2.36 MiB), so the host part is 5.89 MB against 7.96 MB for 0.2.0: the size-optimised profile shrank all host code (old and new) by ~2.8 MB, which pays for the guest and the new tier 1/2, swarm and daemon code. `--features oci` adds about 150 KB |

## Per-command overhead and setup (wall time of a whole `zcode run` against a local fake model server)

| | tier 0 | tier 1 | tier 2 |
|---|---|---|---|
| `run` with one `bash true` | ~12 ms | ~14 ms (+2..20 ms) | ~40 ms (**+~30 ms**, container create + start + guest connect, from scratch, warm page cache) |
| extra cost per further command | 2.5 ms (agent + fake-LLM round trip dominates) | +0.0..0.4 ms | within noise of tier 0 (the guest RPC adds ~0.1-0.3 ms; the measurement noise is larger) |

Tier 1 setup is the Landlock ruleset (about 25 raw syscalls in the forked child) plus the seccomp filter: microseconds to a few hundred microseconds. Tier 2 create-from-scratch is **~30-50 ms** here, well below the 150-250 ms first estimated; the cold-cache first overlay mount took 667 ms once (page cache), then ~11-20 ms. A bigger image only affects first-touch I/O, not create time (see below).

## Root filesystem prototype (`RootfsProvider`): time to give one agent a private writable `/`

Mount (or copy) + create one file, 5 runs each (`unshare -Urm`, WSL2 ext4), ms:

| mechanism | 2 MB base (3 files) | 1.3 GB base (30,620 files) |
|---|---|---|
| (a) kernel overlayfs in a user+mount ns | 11-13 (first run 667, cold) | 10-20 (first run 45) |
| (b) `fuse-overlayfs` | 9-19 | 14-17 (plus a userspace daemon per agent and slower data path) |
| (c) `cp -a --reflink=auto` | 5-7 | **1,700-2,400** (no reflink on ext4) |

**Chosen: (a)**, constant-time regardless of image size, no extra process, no daemon, upper layer is a plain directory in the per-agent state dir. (b) is the automatic fallback when overlayfs cannot be mounted in a userns; (c) is last-resort (`container.rootfs: "copy"`), slow start for big images.

## Memory per extra agent (Pss, kB; N workers each running `sleep`)

| | 1 | 4 | 8 | per extra agent |
|---|---|---|---|---|
| in-process workers, tier 0 (zcode RSS) | ~5,400-6,800 | ~5,900-7,500 | ~6,200-7,400 | **~0.1 MB** |
| container workers, tier 2 (zcode + guest init + guest worker + `zcode __ns` supervisor) | ~12,800 | ~16,900 | ~21,600 | **~1.0-1.3 MB** (guest 2 procs + supervisor ≈ 1 MB; zcode itself grows ~0.2 MB per agent) |

(The first estimate, a few to tens of MB per agent, was pessimistic for busybox-class images; a real image's processes cost what they cost.)

## Search throughput at `opt-level = "s"` vs 3 (`bench/sandbox/toolcpu.py`)

24 full scans of a 47 MB, 2400-file tree through the `grep` tool plus `glob` calls, median of 7 `zcode run`s, tier 0:

| pattern | `opt-level = 3` | `opt-level = "s"` |
|---|---|---|
| literal prefixes (`(qqq|zzz)d+...`, never matches) | 2120 ms | 2110 ms |
| no literals (`[a-z]+[0-9]{4}[a-z]+...`, regex-engine bound) | 3000 ms | 2892 ms |

No measurable slowdown (the first, noisy runs suggested -30%; repeated quiet runs showed it was noise), so the whole release profile is size-optimised.

## `zcode image pull` (feature `oci`)

Docker Hub, `alpine:3.20` (1 layer, 3.6 MB), cold cache: 4.2 s wall (network bound), 0.7 s user. A repeat pull is served from the blob cache with no layer download. `busybox --platform linux/arm64` selects the arm64 image from the multi-arch index. A pulled Alpine rootfs boots as a tier 2 container.

## How these were produced

`hyperfine -N --warmup 30 --runs 300 '<bin> --version'`; idle RSS from `/proc/<pid>/status` of the REPL with stdin held open; prompt size from the request body captured by a local fake server (`system` + `tools` chars / 4); per-command and create times from 8 (1 command) and 3 (101 commands) full `zcode run` invocations with the median; rootfs numbers from `unshare -Urm` + `mount -t overlay` / `fuse-overlayfs` / `cp -a`; memory from `ps`/`smaps_rollup` while workers sleep. The scripts are in `bench/sandbox/` (they expect the baseline binary at `~/zcode-base` and the new release build at `~/zt/release/zcode`; edit the paths at the top). The in-repo tests (`cargo test --workspace`) cover correctness, not timing.
