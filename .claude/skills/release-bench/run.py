#!/usr/bin/env python3
"""One command for the whole release benchmark: fetch -> bench -> release-page markdown.

Usage: python run.py [TAG] [--out DIR] [--wait SECONDS] [--suite startup local ...] [--threshold 5]

No shell expansion anywhere (works the same in bash and zsh). Uses bench/venv's python when present.
Progress and per-step timings go to stderr; the final markdown goes to stdout and to DIR/release-bench.md.
"""
import argparse, subprocess, sys, tempfile, time
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[2]
BENCH = ROOT / "bench"
BINS_CACHE = Path.home() / ".cache" / "zcode-release-bench" / "bins"  # baselines reused across runs


def step(name, t0):
    print(f"[{time.perf_counter() - t0:6.1f}s] {name}", file=sys.stderr, flush=True)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("tag", nargs="?")
    ap.add_argument("--out", help="output dir (default: new temp dir)")
    ap.add_argument("--wait", type=int, default=900, help="max seconds to wait for the new release assets")
    ap.add_argument("--suite", nargs="+", default=["startup", "local"])
    ap.add_argument("--threshold", type=float, default=5.0)
    a = ap.parse_args()

    py = str(BENCH / "venv" / "bin" / "python")
    if not Path(py).exists():
        py = sys.executable
    out = Path(a.out) if a.out else Path(tempfile.mkdtemp(prefix="release-bench-"))
    out.mkdir(parents=True, exist_ok=True)
    t0 = time.perf_counter()

    cmd = [sys.executable, str(HERE / "fetch_release_bins.py"), "--out", str(BINS_CACHE), "--wait", str(a.wait)]
    if a.tag:
        cmd.insert(2, a.tag)
    r = subprocess.run(cmd, cwd=ROOT, stdout=subprocess.PIPE, text=True)  # stderr passes through (plan, retries)
    if r.returncode:
        sys.exit(f"fetch failed (rc={r.returncode}); not falling back to a local build")
    specs = [l for l in r.stdout.splitlines() if "=" in l]
    names = [s.split("=", 1)[0] for s in specs]
    step(f"fetched {len(specs)} binaries: {', '.join(names)}", t0)

    res_dir = out / "results"
    bench = [py, "bench.py", "--bin", *specs, "--tools", *names, "--suite", *a.suite, "--out", str(res_dir)]
    r = subprocess.run(bench, cwd=BENCH, stdout=subprocess.DEVNULL)
    if r.returncode:
        sys.exit(f"bench failed (rc={r.returncode})")
    latest = max(res_dir.glob("*.json"), key=lambda p: p.stat().st_mtime)
    step(f"bench done ({latest.name})", t0)

    r = subprocess.run([sys.executable, str(HERE / "release_notes.py"), str(latest), "--threshold", str(a.threshold)],
                       cwd=ROOT, stdout=subprocess.PIPE, text=True)
    if r.returncode:
        sys.exit(f"report failed (rc={r.returncode})")
    md = out / "release-bench.md"
    md.write_text(r.stdout)
    step(f"report written: {md}", t0)
    print(r.stdout)


if __name__ == "__main__":
    main()
