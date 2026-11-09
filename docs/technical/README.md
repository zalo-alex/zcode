# Technical documentation

For people who build on, operate or contribute to zcode. If you just want to use it, read the [user guide](../user/README.md).

| Document | What is in it |
|---|---|
| [PROTOCOL.md](PROTOCOL.md) | The reference: every command and flag, config keys and layers, environment variables, `--format json` events, sessions, tools, the swarm, the tier 0/1 sandbox, exit codes |
| [TIER2.md](TIER2.md) | Tier 2 containers: requirements, config, images (import and in-process pull), the container layout, networking, lifecycle, and the guest protocol |
| [DAEMON.md](DAEMON.md) | `zcode serve`: the socket protocol, operations, events, limits, and how `zcode run` becomes a thin client |
| [KNOWN_GAPS.md](KNOWN_GAPS.md) | Design notes, what is not implemented, inherent limits of the sandbox, and the idle-memory accounting |
| [../../bench/BENCH.md](../../bench/BENCH.md) | Benchmark note: startup, memory, binary size, per-command overhead, container create time |

## Source layout

```
src/                     the agent: loop, REPL, swarm, daemon (serve.rs, client.rs), config, sessions, events
crates/zcode-tools/      the tools and the Sandbox abstraction (tier 0/1 local, tier 2 container host side)
crates/zcode-guest/      the static musl init + tool server that runs inside a tier 2 container
tests/                   integration tests against a fake OpenAI server (cli, swarm, container, daemon)
bench/                   benchmarks and the scripts that produced bench/BENCH.md
```

## Building and testing

```bash
cargo build --release                    # embeds the guest (needs: rustup target add $(uname -m)-unknown-linux-musl)
cargo build --release --features oci     # also the in-process `zcode image pull` / `image gc`
cargo test --workspace                   # tier 2 tests skip with a visible reason without crun / busybox / user namespaces
cargo clippy --workspace --all-targets -- -D warnings
```

`ZCODE_SKIP_GUEST=1` builds without the container guest (tier 2 is then unavailable); `ZCODE_GUEST_BIN=<path>` embeds a prebuilt one.
