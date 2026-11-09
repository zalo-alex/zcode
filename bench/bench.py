#!/usr/bin/env python3
"""zcode vs opencode benchmark suite.

Suites:
  startup   --version latency + RSS
  overhead  fixed prompt cost: system prompt / tool schema size & prompt tokens for a trivial 'PONG' request
  tasks     agentic coding tasks (easy/medium/hard) with hidden programmatic checkers
  prompts   prompt-behaviour cases (reasoning, format, codegen, long-context, multi-turn, injection, tool choice)

All LLM traffic goes through a local metering proxy (proxy.py) so both tools are measured identically:
requests, prompt/completion tokens, TTFT, generation speed. Same upstream model for both.
"""
import argparse
import random
import json
import os
import shutil
import statistics as st
import subprocess
import tempfile
import threading
import time
from pathlib import Path

import psutil

from stub import Stub

from proxy import Meter
from prompts import CASES, NT
from tasks import TASKS

ROOT = Path(__file__).resolve().parent
ZCODE = ROOT.parent / "target" / "release" / "zcode"
BINS: dict[str, Path] = {}  # extra zcode binaries: tool name -> path (--bin NAME=PATH); name must start with "zcode"


def is_zcode(tool: str) -> bool:
    return tool.startswith("zcode")


def zbin(tool: str) -> Path:
    return BINS.get(tool, ZCODE)
OC_CONFIG = Path.home() / ".config/opencode/opencode.json"


# ------------------------------------------------------------------ setup
def upstream_cfg():
    cfg = json.loads(OC_CONFIG.read_text())
    prov = next(iter(cfg["provider"].values()))
    return cfg, prov["options"]["baseURL"]


def make_env(tool: str, cwd: Path, proxy_url: str, base_cfg: dict, data_home: Path) -> dict:
    env = dict(os.environ)
    env["PWD"] = str(cwd)  # opencode trusts $PWD; without this it edits the harness cwd
    if is_zcode(tool):
        env["ZCODE_BASE_URL"] = proxy_url
        env["XDG_DATA_HOME"] = str(data_home)
    else:
        cfg = json.loads(json.dumps(base_cfg))
        for p in cfg["provider"].values():
            p["options"]["baseURL"] = proxy_url
        env["OPENCODE_CONFIG_CONTENT"] = json.dumps(cfg)
        env["OPENCODE_DISABLE_AUTOUPDATE"] = "1"
    return env


def cmd(tool: str, prompt: str, cwd: Path, cont: bool = False) -> list[str]:
    if is_zcode(tool):
        c = [str(zbin(tool)), "--yes", "-C", str(cwd)]
        if cont:
            c.append("--resume")
        return c + ["-p", prompt]
    c = ["opencode", "run", "--auto", "--standalone"]
    if cont:
        c.append("--continue")
    return c + [prompt]


def version_cmd(tool: str) -> list[str]:
    return [str(zbin(tool)) if is_zcode(tool) else tool, "--version"]


# ------------------------------------------------------------------ measurement
def run_measured(c: list[str], cwd: Path, env: dict, timeout: float) -> dict:
    t0 = time.perf_counter()
    p = subprocess.Popen(c, cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         stdin=subprocess.DEVNULL, text=True, errors="replace")
    ps = psutil.Process(p.pid)
    chunks: list[str] = []
    th = threading.Thread(target=lambda: chunks.append(p.stdout.read()), daemon=True)
    th.start()
    peak, timed_out, cpu = 0, False, 0.0
    while True:
        try:
            pid, status, ru = os.wait4(p.pid, os.WNOHANG)
        except ChildProcessError:
            ru, pid = None, p.pid
        if pid:
            if ru:
                cpu = ru.ru_utime + ru.ru_stime
                peak = max(peak, ru.ru_maxrss * 1024)
            p.returncode = os.waitstatus_to_exitcode(status) if ru else p.returncode
            break
        try:
            rss = sum(x.memory_info().rss for x in [ps, *ps.children(recursive=True)])
            peak = max(peak, rss)
        except psutil.Error:
            pass
        if time.perf_counter() - t0 > timeout:
            timed_out = True
            for x in [*ps.children(recursive=True), ps]:
                try:
                    x.kill()
                except psutil.Error:
                    pass
        time.sleep(0.02)
    th.join(3)
    return {"time": time.perf_counter() - t0, "cpu": cpu, "rss_mb": peak / 1e6, "rc": p.returncode,
            "timeout": timed_out, "out": "".join(chunks)}


def llm_stats(recs: list[dict]) -> dict:
    rec = [r for r in recs if r.get("status") == 200 and "n_messages" in r]
    if not rec:
        return {"requests": 0, "prompt_tokens": 0, "completion_tokens": 0, "ttft": None, "tok_s": None, "tool_calls": 0}
    pt = sum(r["prompt_tokens"] if r.get("prompt_tokens") else r.get("est_prompt_tokens", 0) for r in rec)
    ct = sum(r["completion_tokens"] if r.get("completion_tokens") else r.get("out_chars", 0) // 4 for r in rec)
    gen_t = sum(max(r["latency"] - r["ttft"], 1e-3) for r in rec)
    return {"requests": len(rec), "prompt_tokens": pt, "completion_tokens": ct,
            "ttft": st.median(r["ttft"] for r in rec), "tok_s": ct / gen_t if gen_t else None,
            "tool_calls": sum(r.get("tool_calls", 0) for r in rec)}


class Bench:
    def __init__(self, a):
        self.a = a
        self.base_cfg, self.upstream = upstream_cfg()

    def new_meter(self):
        m = Meter(self.upstream)
        return m, m.start()

    @staticmethod
    def close_meter(m):
        if m.server:
            m.server.shutdown()
            m.server.server_close()

    def one(self, tool: str, turns: list[str], setup, check, timeout: float, isolate=True):
        d = Path(tempfile.mkdtemp(prefix=f"bench_{tool}_"))
        data = Path(tempfile.mkdtemp(prefix="bench_data_"))
        try:
            setup(d)
            meter, proxy_url = self.new_meter()
            env = make_env(tool, d, proxy_url, self.base_cfg, data)
            outs, tot = [], {"time": 0.0, "cpu": 0.0, "rss_mb": 0.0}
            timed_out, rc = False, 0
            for i, p in enumerate(turns):
                r = run_measured(cmd(tool, p, d, cont=i > 0), d, env, timeout)
                outs.append(r["out"])
                tot["time"] += r["time"]
                tot["cpu"] += r["cpu"]
                tot["rss_mb"] = max(tot["rss_mb"], r["rss_mb"])
                timed_out |= r["timeout"]
                rc = r["rc"]
            try:
                ok = bool(check(d, outs if len(turns) > 1 or True else outs)) and not timed_out
            except Exception as e:
                ok = False
            s = llm_stats(meter.snapshot())
            res = {"ok": ok, "rc": rc, "timeout": timed_out, **tot, **s}
            if s["requests"] == 0:
                res["warn"] = "no LLM traffic seen via proxy"
            if not ok:
                res["out_tail"] = strip(outs[-1])[-500:]
            return res
        finally:
            if "meter" in locals():
                self.close_meter(meter)
            shutil.rmtree(d, ignore_errors=True)
            shutil.rmtree(data, ignore_errors=True)


def strip(s):
    """Remove ANSI codes and opencode's per-step header (`> build · model`) so checkers see only the answer."""
    import re
    s = re.sub(r"\x1b\[[0-9;]*m", "", s)
    return re.sub(r"(?m)^> \S+ · \S+\s*$\n?", "", s)


# ------------------------------------------------------------------ suites
def spawn(c, env, cwd, stub=None, timeout=60):
    """One process run. Returns wall/cpu ms, max RSS MB, minor faults, rc, stdout and (stub) time-to-first-request."""
    if stub:
        stub.reset()
    t0 = time.perf_counter()
    p = subprocess.Popen(c, cwd=cwd, env=env, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.DEVNULL)
    out = p.stdout.read()  # drain so a big reply can't block the child
    _, status, ru = os.wait4(p.pid, 0)
    wall = (time.perf_counter() - t0) * 1000
    p.returncode = 0
    r = {"wall_ms": wall, "cpu_ms": (ru.ru_utime + ru.ru_stime) * 1000, "rss_mb": ru.ru_maxrss / 1024,
         "faults": ru.ru_minflt, "rc": os.waitstatus_to_exitcode(status), "out": out.decode("utf-8", "replace")}
    if stub:
        with stub.lock:
            reqs = list(stub.requests)
        r["ttfr_ms"] = (reqs[0]["t"] - t0) * 1000 if reqs else None
        r["shape"] = {k: reqs[0].get(k) for k in ("sys_chars", "n_tools", "tools_chars", "req_bytes")} if reqs else None
    return r


def interleaved(tools, rounds, warmup, one):
    """Run one(tool) for every tool in `rounds` rounds, tool order shuffled each round.

    Interleaving means slow drift (thermal, background load, page cache) hits all versions equally, so
    differences between versions are not an artefact of *when* they ran. Returns {tool: [result, ...]}.
    """
    rng = random.Random(7)
    res = {t: [] for t in tools}
    for i in range(warmup + rounds):
        order = list(tools)
        rng.shuffle(order)
        for t in order:
            r = one(t)
            if i >= warmup:
                res[t].append(r)
    return res


def metric_lists(runs, keys=("wall_ms", "cpu_ms", "rss_mb", "faults", "ttfr_ms")):
    ok = [r for r in runs if r["rc"] == 0]
    out = {k: [round(r[k], 3) for r in ok if r.get(k) is not None] for k in keys}
    out = {k: v for k, v in out.items() if v}
    out["failed"] = len(runs) - len(ok)
    return out


def suite_startup(a):
    """No-LLM cold-start commands, sampled in interleaved rounds. Raw samples are kept for the report."""
    data = Path(tempfile.mkdtemp(prefix="bench_su_"))
    cmds = {"version": ["--version"], "help": ["--help"], "sessions": ["sessions"]}
    out = {t: {"samples": {}} for t in a.tools}
    try:
        for name, args in cmds.items():
            tools = [t for t in a.tools if is_zcode(t) or name == "version"]
            envs = {t: {**os.environ, "XDG_DATA_HOME": str(data / t)} for t in tools}
            runs = interleaved(tools, a.startup_runs, 5,
                               lambda t: spawn(version_cmd(t)[:1] + args, envs[t], ROOT))
            for t in tools:
                out[t]["samples"][name] = metric_lists(runs[t])
        for t in a.tools:  # legacy keys used by report()
            v = out[t]["samples"]["version"]
            w = sorted(v["wall_ms"])
            out[t].update({"ms_median": st.median(w), "ms_min": w[0], "ms_p95": w[int(len(w) * 0.95) - 1], "rss_mb": st.median(v["rss_mb"])})
    finally:
        shutil.rmtree(data, ignore_errors=True)
    return out


def suite_local(a):
    """Agent loop against an instant local stub LLM: spawn -> request -> streamed reply -> exit.

    No network and no tokens spent, so wall/CPU/RSS/time-to-first-request reflect the client only.
    Scenarios: pong (tiny reply) and stream (200 KB reply in 4-char SSE chunks = parsing/rendering cost).
    """
    tools = [t for t in a.tools if is_zcode(t)]
    scen = {"pong": ("Say only PONG.", "PONG", "PONG"), "stream": ("Write a long text.", "lorem ipsum " * 16000, "lorem")}
    stub = Stub()
    url = stub.start()
    root = Path(tempfile.mkdtemp(prefix="bench_loc_"))
    out = {t: {} for t in tools}
    try:
        for name, (prompt, reply, expect) in scen.items():
            stub.set_reply(reply)

            def one(t):
                d = Path(tempfile.mkdtemp(dir=root))
                env = make_env(t, d, url, {}, d / "data")
                r = spawn(cmd(t, prompt, d), env, d, stub)
                if expect not in r["out"]:
                    r["rc"] = r["rc"] or 1
                shutil.rmtree(d, ignore_errors=True)
                return r

            runs = interleaved(tools, a.local_runs, 3, one)
            for t in tools:
                ok = [r for r in runs[t] if r["rc"] == 0]
                out[t][name] = metric_lists(runs[t])
                if ok and ok[0].get("shape"):
                    out[t][name]["shape"] = ok[0]["shape"]
    finally:
        stub.stop()
        shutil.rmtree(root, ignore_errors=True)
    return out


def environment(a):
    def rd(p):
        try:
            return Path(p).read_text().strip()
        except OSError:
            return None
    cpu = next((l.split(":", 1)[1].strip() for l in (rd("/proc/cpuinfo") or "").splitlines() if l.startswith("model name")), None)
    bins = {}
    for t in a.tools:
        if is_zcode(t):
            p = zbin(t)
            ver = subprocess.run([str(p), "--version"], capture_output=True, text=True).stdout.strip()
            bins[t] = {"size_bytes": p.stat().st_size, "version": ver}
    return {"cpu": cpu, "cores": os.cpu_count(), "governor": rd("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor"),
            "loadavg_start": os.getloadavg(), "binaries": bins}


def suite_overhead(b: Bench):
    """Trivial prompt, no tools needed: first request shows fixed system+tools overhead."""
    out = {}
    for tool in b.a.tools:
        d = Path(tempfile.mkdtemp(prefix="bench_oh_"))
        data = Path(tempfile.mkdtemp(prefix="bench_data_"))
        try:
            meter, url = b.new_meter()
            env = make_env(tool, d, url, b.base_cfg, data)
            r = run_measured(cmd(tool, "Say only PONG.", d), d, env, 120)
            b.close_meter(meter)
            recs = [x for x in meter.snapshot() if "n_messages" in x]
            first = max(recs, key=lambda x: (x.get("n_tools", 0), x.get("req_bytes", 0))) if recs else {}  # main agent request, not title-gen
            out[tool] = {"requests": len(recs), "sys_chars": first.get("sys_chars"), "n_tools": first.get("n_tools"),
                         "tools_chars": first.get("tools_chars"), "req_bytes": first.get("req_bytes"),
                         "prompt_tokens": first.get("prompt_tokens") or first.get("est_prompt_tokens"),
                         "ttft": first.get("ttft"), "wall_s": r["time"], "rss_mb": r["rss_mb"]}
        finally:
            shutil.rmtree(d, ignore_errors=True)
            shutil.rmtree(data, ignore_errors=True)
    return out


def run_jobs(a, jobs, fmt):
    """jobs: list of (key, tool, fn). Runs with a thread pool; returns {key: {tool: [runs]}}."""
    from concurrent.futures import ThreadPoolExecutor, as_completed
    out: dict = {}
    with ThreadPoolExecutor(max_workers=max(1, a.parallel)) as ex:
        futs = {ex.submit(fn): (key, tool, i) for key, tool, i, fn in jobs}
        for f in as_completed(futs):
            key, tool, i = futs[f]
            r = f.result()
            print(fmt(key, tool, i, r), flush=True)
            out.setdefault(key, {}).setdefault(tool, []).append(r)
    return out


def suite_tasks(b: Bench):
    a = b.a
    tasks = [t for t in TASKS if (not a.tasks or t.name in a.tasks) and (not a.levels or t.level in a.levels)]
    jobs = [(t.name, tool, i, lambda t=t, tool=tool: b.one(tool, [t.prompt], t.setup, lambda d, o, t=t: t.check(d, strip(o[-1])), a.timeout))
            for t in tasks for i in range(a.runs) for tool in a.tools]
    got = run_jobs(a, jobs, lambda k, tool, i, r: f"  {k:18} {tool:9} run{i + 1}: {'PASS' if r['ok'] else 'FAIL'} "
                   f"{r['time']:.1f}s req={r['requests']} tok={r['prompt_tokens']}+{r['completion_tokens']} {r['rss_mb']:.0f}MB"
                   + (f" [{r['warn']}]" if 'warn' in r else ""))
    return {t.name: {"level": t.level, "tools": got[t.name]} for t in tasks}


def suite_prompts(b: Bench):
    a = b.a
    cases = [c for c in CASES if not a.cases or c.name in a.cases]
    jobs = [(c.name, tool, i, lambda c=c, tool=tool: b.one(tool, c.turns, c.setup, lambda d, o, c=c: c.check(d, o), a.timeout))
            for c in cases for i in range(a.runs) for tool in a.tools]
    got = run_jobs(a, jobs, lambda k, tool, i, r: f"  {k:22} {tool:9} run{i + 1}: {'PASS' if r['ok'] else 'FAIL'} "
                   f"{r['time']:.1f}s ttft={r['ttft'] and round(r['ttft'], 2)} tok={r['prompt_tokens']}+{r['completion_tokens']}")
    return {c.name: {"category": c.category, "tools": got[c.name]} for c in cases}


# ------------------------------------------------------------------ report
def md(headers, rows):
    w = [max(len(str(x)) for x in col) for col in zip(headers, *rows)]
    line = lambda r: "| " + " | ".join(str(x).ljust(n) for x, n in zip(r, w)) + " |"
    return "\n".join([line(headers), "|" + "|".join("-" * (n + 2) for n in w) + "|", *map(line, rows)])


def f(x, n=1):
    return "-" if x is None else f"{x:.{n}f}"


def agg(runs):
    ok = sum(r["ok"] for r in runs)
    return {"pass": f"{ok}/{len(runs)}", "ok": ok, "n": len(runs), "time": st.median(r["time"] for r in runs),
            "cpu": st.median(r["cpu"] for r in runs), "rss": max(r["rss_mb"] for r in runs),
            "req": st.mean(r["requests"] for r in runs), "pt": st.mean(r["prompt_tokens"] for r in runs),
            "ct": st.mean(r["completion_tokens"] for r in runs)}


def report(res: dict, tools: list[str]) -> str:
    L = ["# zcode vs opencode benchmark", ""]
    if "startup" in res:
        L += ["## Startup (`--version`)", md(["tool", "median ms", "min ms", "p95 ms", "RSS MB"],
              [[t, f(s["ms_median"]), f(s["ms_min"]), f(s["ms_p95"]), f(s["rss_mb"])] for t, s in res["startup"].items()]), ""]
    if "local" in res:
        rows = [[t, sc, f(m["wall_ms"] and st.median(m["wall_ms"])), f(st.median(m["cpu_ms"])), f(st.median(m["rss_mb"])),
                 f(st.median(m["ttfr_ms"]) if m.get("ttfr_ms") else None), m["failed"]]
                for t, d in res["local"].items() for sc, m in d.items() if m.get("wall_ms")]
        L += ["## Local stub LLM (no network)", md(["tool", "scenario", "wall ms", "cpu ms", "RSS MB", "to 1st request ms", "failed"], rows), ""]
    if "overhead" in res:
        L += ["## Fixed prompt overhead (request for 'Say only PONG.')", md(
            ["tool", "system chars", "tools", "tool schema chars", "request bytes", "prompt tokens", "TTFT s", "wall s", "RSS MB"],
            [[t, o["sys_chars"], o["n_tools"], o["tools_chars"], o["req_bytes"], o["prompt_tokens"], f(o["ttft"], 2), f(o["wall_s"]), f(o["rss_mb"], 0)]
             for t, o in res["overhead"].items()]), ""]
    for key, label, grp in (("tasks", "Agentic tasks", "level"), ("prompts", "Prompt cases", "category")):
        if key not in res:
            continue
        rows = []
        for name, e in res[key].items():
            for t in tools:
                if t in e["tools"]:
                    g = agg(e["tools"][t])
                    rows.append([e[grp], name, t, g["pass"], f(g["time"]), f(g["cpu"], 2), f(g["rss"], 0), f(g["req"]), f(g["pt"], 0), f(g["ct"], 0)])
        L += [f"## {label}", md([grp, "case", "tool", "pass", "median s", "cpu s", "peak RSS MB", "LLM reqs", "prompt tok", "compl tok"], rows), ""]
        # per-group + totals
        grows = []
        groups = sorted({e[grp] for e in res[key].values()})
        for g_ in groups + ["ALL"]:
            for t in tools:
                runs = [r for e in res[key].values() if (g_ == "ALL" or e[grp] == g_) and t in e["tools"] for r in e["tools"][t]]
                if not runs:
                    continue
                ok = sum(r["ok"] for r in runs)
                tot_tok = sum(r["prompt_tokens"] + r["completion_tokens"] for r in runs)
                grows.append([g_, t, f"{ok}/{len(runs)} ({100 * ok / len(runs):.0f}%)", f(sum(r["time"] for r in runs)),
                              f(st.mean(r["rss_mb"] for r in runs), 0), tot_tok, f(tot_tok / ok, 0) if ok else "-",
                              f(sum(r["time"] for r in runs) / ok) if ok else "-"])
        L += [f"### {label}: summary by {grp}", md([grp, "tool", "pass rate", "total s", "mean RSS MB", "total tokens", "tokens/pass", "s/pass"], grows), ""]
    L += ["_Notes: tokens come from upstream `usage` (est. chars/4 if absent). Wall time is dominated by model latency; use --runs >= 3._"]
    return "\n".join(L)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tools", nargs="+", default=["zcode", "opencode"])
    ap.add_argument("--suite", nargs="+", default=["startup", "local", "overhead", "tasks", "prompts"],
                    choices=["startup", "local", "overhead", "tasks", "prompts"])
    ap.add_argument("--levels", nargs="*", help="easy medium hard")
    ap.add_argument("--tasks", nargs="*")
    ap.add_argument("--cases", nargs="*")
    ap.add_argument("--runs", type=int, default=3)
    ap.add_argument("--startup-runs", type=int, default=100, help="interleaved rounds per startup command")
    ap.add_argument("--local-runs", type=int, default=30, help="interleaved rounds per local-stub scenario")
    ap.add_argument("--timeout", type=float, default=300)
    ap.add_argument("--parallel", type=int, default=1, help="concurrent runs for tasks/prompts suites (faster; wall-time columns get noisier)")
    ap.add_argument("--bin", nargs="*", default=[], metavar="NAME=PATH",
                    help="extra zcode binaries to benchmark, e.g. zcode-v0.2.0=/tmp/x/zcode; use NAME in --tools")
    ap.add_argument("--out", default=str(ROOT / "results"))
    a = ap.parse_args()

    for spec in a.bin:
        name, _, path = spec.partition("=")
        if not name.startswith("zcode") or not Path(path).is_file():
            raise SystemExit(f"bad --bin {spec!r}: need zcodeNAME=/path/to/binary")
        BINS[name] = Path(path)
    if "zcode" in a.tools and "zcode" not in BINS and not ZCODE.exists():
        raise SystemExit(f"build first: cargo build --release ({ZCODE})")
    for t in a.tools:
        if not is_zcode(t) and not shutil.which(t):
            raise SystemExit(f"{t} not on PATH")
    b = Bench(a) if set(a.suite) & {"overhead", "tasks", "prompts"} else None
    res: dict = {"env": environment(a)}
    if "startup" in a.suite:
        print("== startup ==", flush=True)
        res["startup"] = suite_startup(a)
    if "local" in a.suite:
        print("== local (stub LLM) ==", flush=True)
        res["local"] = suite_local(a)
    if "overhead" in a.suite:
        print("== overhead ==", flush=True)
        res["overhead"] = suite_overhead(b)
    if "tasks" in a.suite:
        print("== tasks ==", flush=True)
        res["tasks"] = suite_tasks(b)
    if "prompts" in a.suite:
        print("== prompts ==", flush=True)
        res["prompts"] = suite_prompts(b)
    res["env"]["loadavg_end"] = os.getloadavg()
    out = Path(a.out)
    out.mkdir(exist_ok=True)
    stamp = time.strftime("%Y%m%d-%H%M%S")
    (out / f"{stamp}.json").write_text(json.dumps(res, indent=2))
    rep = report(res, a.tools)
    (out / f"{stamp}.md").write_text(rep)
    print("\n" + rep + f"\n\nsaved {out}/{stamp}.{{json,md}}")


if __name__ == "__main__":
    main()
