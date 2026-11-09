---
name: release-bench
description: Benchmark a published zcode release against its previous patch, minor and major releases, using the binaries built by GitHub Actions (not local builds). Use after a release tag was pushed and the release workflow finished, or when asked to compare release versions / check for regressions between releases.
---

# release-bench

Compares up to 4 zcode binaries with `bench/bench.py`: **new** (the release just published) vs the latest
**patch**, **minor** and **major** baselines. All binaries are downloaded from GitHub Releases. Never `cargo build`.

Baselines relative to NEW (`vX.Y.Z`): patch = previous release in `X.Y`; minor = latest release of the previous
minor line; major = latest release of the previous major line. Missing or duplicate baselines are skipped
(e.g. first release of a 0.x series has no major), so you may get 2-4 binaries. Say which were skipped.

## Run it (one command)

```bash
python /home/alex/dev/zcode/.claude/skills/release-bench/run.py [TAG]
```

Run exactly that, as a single call, with a long Bash timeout (`600000`; ~1-2 min normally, plus up to 15 min if the
release assets are still being built). It does everything: picks the tag (given, else highest `v*`), downloads the
binaries (polls for the new release's assets, sha256-verifies, caches baselines in `~/.cache/zcode-release-bench`),
runs `bench.py --suite startup local`, and generates the release markdown. It uses `bench/venv`'s python itself.

- stdout = the release-page markdown. stderr = plan and per-step timings. Files go to a fresh temp dir
  (`release-bench.md`, `results/*.json`, path printed on stderr); `--out DIR` to choose one.
- Options: `--wait SECONDS`, `--suite ...`, `--threshold PCT` (default 5).
- **Do not hand-roll the steps** with shell variables, `$(cat ...)`, `${(f)...}` or `rm -rf`. The old multi-step flow
  broke under zsh (no word-splitting: two versions merged into one bogus row) and got blocked by the command safety
  check. Don't delete or clean old results: every run uses a new directory.
- If the new asset never appears the command fails: tell the user to check the `release` workflow run in Actions.
  Never fall back to a local build.

## What it measures
- `startup`: `--version`, `--help`, `sessions`, 100 interleaved rounds each (wall, CPU, RSS, faults).
- `local`: full agent run against an instant local stub LLM (`bench/stub.py`), 30 interleaved rounds, scenarios `pong`
  and `stream`; also time-to-first-request and prompt footprint. No network, API key or opencode needed.
- Interleaved = each round runs every version once in random order, so system drift hits all versions equally.
- `tasks` / `prompts` / `overhead` need a real upstream (billed, slow, noisy): only if the user asks, via
  `bench.py` directly.

## Output
Headline (`N slower · M faster · K within 5%` per baseline, no emoji) + **one short visible table** (7 rows: startup, short run, long streamed reply wall + CPU,
memory, binary size, prompt size; median per version and Δ vs each baseline) + **one collapsed `<details>` block** with the complete table (every benchmark), per-benchmark text charts and full
distribution tables with paired-bootstrap CIs, and a caveats line only when relevant (high load, `powersave`
governor). No method section. Deltas are colored with LaTeX (`$\color{...}{+6.8\%}$`), graded by size of the median change only: grey <1%, amber/light green 1-5%, red/green >=5% (red worse, green better). The 95% CIs are in the details tables. If the delta cells show raw `$\color...$` where it is pasted, math is not rendered there: tell the user.

Return the markdown to the user in a fenced ```` ```markdown ```` block, ready to paste into the release description,
plus the file path as a **clickable markdown link**, e.g. `[release-bench.md](/abs/path/release-bench.md)` (absolute path, link text = file name),
never in backticks. Add 1-3 plain lines outside the block: which baselines were skipped and the key takeaway. If
caveats are present or only a few-% regressions are flagged, offer a re-run before calling it a regression. Do not
edit the release (`gh release edit`, API) unless asked. Nothing is written to the repo unless asked.

## Notes
- Linux only (matches the release assets `zcode-linux-{x86_64,aarch64}.tar.gz`); arch chosen from `uname -m`.
- Binaries are sha256-verified against the `.sha256` release asset.
- Private repo: export `GITHUB_TOKEN` (the script sends it as a bearer token).
- Old releases may lack flags the bench relies on (`--yes`, `-C`, `-p`, `sessions`). Failed runs are counted in the JSON
  (`failed`) and excluded from stats; if a whole benchmark fails for a version it is omitted from the report, so say so.
- Charts are text (render everywhere, including GitHub release pages); no images to host.
