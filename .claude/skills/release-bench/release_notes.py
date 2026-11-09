#!/usr/bin/env python3
"""Turn a bench.py results JSON into a markdown section for the GitHub release page.

Usage: release_notes.py RESULTS.json [--threshold 5]

Tool names in the JSON must be `zcode-<label>-<tag>` (label: new|patch|minor|major), as produced by
fetch_release_bins.py. Output: a headline plus one collapsible <details> block holding the verdict table,
text charts, full distributions per benchmark, and the method/environment. Lower is better everywhere.

A change only counts when it is statistically real: the 95% bootstrap CI of the paired median difference
(rounds are interleaved across versions, so round i of every version shares system state) must exclude 0
AND the change must exceed --threshold percent. Everything else is shown in a neutral style.
"""
import argparse, json, re, sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[3] / "bench"))
from stats import paired_delta, summarize  # noqa: E402

ORDER = {"major": 0, "minor": 1, "patch": 2, "new": 3}
W = 44  # chart width


def parse(res):
    names = set()
    for suite in ("startup", "local"):
        names |= set(res.get(suite, {}))
    for suite in ("tasks", "prompts"):
        for e in res.get(suite, {}).values():
            names |= set(e["tools"])
    out = []
    for n in names:
        m = re.fullmatch(r"zcode-(new|patch|minor|major)-(v[\d.]+)", n)
        if m:
            out.append((ORDER[m[1]], m[1], m[2], n))
    return sorted(out)


def html_code(t):
    return re.sub(r"`([^`]*)`", lambda m: f"<code>{m.group(1)}</code>", t)


def num(x, d=1):
    return "–" if x is None else f"{x:,.{d}f}"


def fmt_pct(p):
    return f"{p:+.1f}%".replace("-", "−")


COLORS = {"worse2": "#cf222e", "worse1": "#bf8700", "same": "#8b949e", "better1": "#2da44e", "better2": "#1a7f37"}


def colored(p, thr, text=None):
    """LaTeX-colored delta, graded by size only: <1% grey, 1%..thr light, >=thr strong; red = worse, green = better."""
    p = round(p, 1) + 0.0  # classify what is displayed; also turns -0.0 into 0.0
    level = "same" if abs(p) < 1 else ("1" if abs(p) < thr else "2")
    key = "same" if level == "same" else ("worse" if p > 0 else "better") + level
    # no "%" and no backslash-escapes inside the math: markdown eats "\%" and a bare % is a LaTeX comment (breaks the brace)
    t = text or (f"{p:+.1f}" if p else "0.0")
    return f"$\\color{{{COLORS[key]}}}{{{t}}}$", key


def verdict(pd, thr):
    """pd = (pct, lo, hi) -> (cell text, class). Colors go by size of the median change only; CI lives in the details."""
    if pd is None:
        return "–", None
    cell, key = colored(pd[0], thr)
    return cell, ("worse" if key.startswith("worse2") else "better" if key.startswith("better2") else "same")


def metrics(res, vers):
    """-> list of (group, title, unit, decimals, {tool: samples}) for every benchmark present in the results."""
    out = []
    names = [v[3] for v in vers]
    s = res.get("startup", {})
    for cmd, label in (("version", "`--version`"), ("help", "`--help`"), ("sessions", "`sessions`")):
        for key, what, unit, d in (("wall_ms", "wall time", "ms", 2), ("cpu_ms", "CPU time", "ms", 2), ("rss_mb", "peak RSS", "MB", 1)):
            data = {n: s[n]["samples"][cmd][key] for n in names if n in s and cmd in s[n].get("samples", {}) and key in s[n]["samples"][cmd]}
            if len(data) == len(names):
                out.append(("Cold start", f"{label} {what}", unit, d, data))
    l = res.get("local", {})
    for sc, label in (("pong", "tiny reply"), ("stream", "200 KB streamed reply")):
        for key, what, unit, d in (("wall_ms", "wall time", "ms", 1), ("cpu_ms", "CPU time", "ms", 1), ("rss_mb", "peak RSS", "MB", 1),
                                   ("ttfr_ms", "time to first LLM request", "ms", 1)):
            data = {n: l[n][sc][key] for n in names if n in l and sc in l[n] and key in l[n][sc]}
            if len(data) == len(names):
                out.append(("Agent loop (local stub LLM)", f"{label}: {what}", unit, d, data))
    return out


def bar_chart(data, vers, unit, d):
    med = {n: summarize(v)["median"] for n, v in data.items()}
    top = max(med.values()) or 1
    lab = max(len(f"{t} {l}") for _, l, t, _ in vers)
    L = []
    for _, l, t, n in vers:
        b = "█" * max(1, round(med[n] / top * W))
        L.append(f"{t + ' ' + l:<{lab}} │{b} {num(med[n], d)} {unit}")
    return L


def box_chart(data, vers, unit, d):
    sm = {n: summarize(v) for n, v in data.items()}
    lo = min(x["p5"] for x in sm.values())
    hi = max(x["p95"] for x in sm.values())
    span = (hi - lo) or 1
    pos = lambda x: max(0, min(W - 1, round((x - lo) / span * (W - 1))))
    lab = max(len(f"{t} {l}") for _, l, t, _ in vers)
    L = []
    for _, l, t, n in vers:
        x = sm[n]
        row = [" "] * W
        for i in range(pos(x["p5"]), pos(x["p95"]) + 1):
            row[i] = "─"
        for i in range(pos(x["p25"]), pos(x["p75"]) + 1):
            row[i] = "▒"
        row[pos(x["median"])] = "┃"
        L.append(f"{t + ' ' + l:<{lab}} │{''.join(row)}│")
    L.append(f"{'':<{lab}}  {num(lo, d)}{' ' * max(1, W - len(num(lo, d)) - len(num(hi, d)))}{num(hi, d)} {unit}")
    return L


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("results")
    ap.add_argument("--threshold", type=float, default=5.0, help="min %% change to call a difference real (default 5)")
    a = ap.parse_args()
    res = json.load(open(a.results))
    vers = parse(res)
    if len(vers) < 2 or vers[-1][1] != "new":
        sys.exit("need the `new` binary and at least one baseline in the results")
    *bases, new = vers
    ms = metrics(res, vers)
    env = res.get("env", {})

    # --- verdicts (paired, per baseline)
    rows, tally = [], {b[3]: {"worse": 0, "better": 0, "same": 0} for b in bases}
    detail = {}
    for group, title, unit, d, data in ms:
        cells = []
        for b in bases:
            pd = paired_delta(data[b[3]], data[new[3]])
            txt, cls = verdict(pd, a.threshold)
            cells.append(txt)
            if cls:
                tally[b[3]][cls] += 1
            detail[(title, b[3])] = pd
        rows.append((group, title, unit, d, cells))

    out = ["## Benchmarks", ""]
    for b in bases:
        t = tally[b[3]]
        out.append(f"- vs **{b[2]}** ({b[1]}): {t['worse']} slower · {t['better']} faster · {t['same']} within {a.threshold:g}%")
    out += ["", f"`{new[2]}` against " + ", ".join(f"{l} `{t}`" for _, l, t, _ in bases)
            + f". Official GitHub Actions binaries, same machine, interleaved runs; medians, lower is better. Delta colors: "
              f"grey under 1%, amber/light green 1-{a.threshold:g}%, red/green {a.threshold:g}% or more (red = worse, green = better).", ""]

    # --- tables: a short one (visible) and the complete one (inside <details>)
    ncol = len(vers) + len(bases)
    head = [f"{l} {t}" if n != new[3] else f"**{t}** (new)" for _, l, t, n in vers]
    thead = ["| benchmark | " + " | ".join(head) + " | " + " | ".join(f"Δ vs {b[2]} (%)" for b in bases) + " |", "|---|" + "---:|" * ncol]
    by_title = {m[1]: (m, r[4]) for m, r in zip(ms, rows)}

    def mrow(label, title):
        if title not in by_title:
            return None
        (_, _, unit, d, data), cells = by_title[title]
        meds = [f"{num(summarize(data[v[3]])['median'], d)} {unit}" for v in vers]
        return f"| {label} | " + " | ".join(meds) + " | " + " | ".join(cells) + " |"

    bins = env.get("binaries", {})
    shapes = {v[3]: (res.get("local", {}).get(v[3], {}).get("pong", {}) or {}).get("shape") for v in vers}

    def drow(label, vals, u):
        dl = []
        for b in bases:
            o, n_ = vals[b[3]], vals[new[3]]
            dl.append("–" if not o else colored((n_ - o) / o * 100, a.threshold)[0])
        return f"| {label} | " + " | ".join(f"{vals[v[3]]:,.0f}{u}" for v in vers) + " | " + " | ".join(dl) + " |"

    size = {n: bins[n]["size_bytes"] / 1024 for n in bins} if len(bins) == len(vers) else None
    foot = {n: shapes[n]["sys_chars"] + shapes[n]["tools_chars"] for n in shapes} if all(shapes.values()) else None

    short = [mrow("Startup", "`--version` wall time"), mrow("Short agent run", "tiny reply: wall time"),
             mrow("Long streamed reply", "200 KB streamed reply: wall time"), mrow("Long reply, CPU", "200 KB streamed reply: CPU time"),
             mrow("Memory (long reply)", "200 KB streamed reply: peak RSS"),
             drow("Binary size", size, " KB") if size else None, drow("Prompt size (system + tools)", foot, " chars") if foot else None]
    out += thead + [r for r in short if r]

    out += ["", "<details>", "<summary><b>Detailed benchmark report</b> (all benchmarks, charts, distributions)</summary>", "",
            "### All benchmarks", ""] + thead
    last = None
    for m, r in zip(ms, rows):
        if m[0] != last:
            out.append(f"| **{m[0]}** |" + " |" * ncol)
            last = m[0]
        out.append(mrow(m[1], m[1]))
    foot_rows = [drow(lab, v, u) for lab, v, u in (
        ("binary size", size, " KB"), ("system prompt", {n: shapes[n]["sys_chars"] for n in shapes} if foot else None, " chars"),
        ("tool schema", {n: shapes[n]["tools_chars"] for n in shapes} if foot else None, " chars"),
        ("first request", {n: shapes[n]["req_bytes"] for n in shapes} if foot else None, " B")) if v]
    if foot_rows:
        out += ["| **Size and prompt footprint** |" + " |" * ncol] + foot_rows
    out += [""]

    # --- per-benchmark charts + distributions
    out += ["", "### Charts and distributions", "",
            "Each chart: `─` p5–p95 · `▒` p25–p75 · `┃` median. Tables give the full distribution; Δ is new vs baseline, "
            "[95% CI] from a paired bootstrap over rounds.", ""]
    for group in dict.fromkeys(m[0] for m in ms):
        out += [f"#### {group}", ""]
        for g, title, unit, d, data in ms:
            if g != group:
                continue
            n_rounds = min(len(v) for v in data.values())
            out += ["<details>", f"<summary>{html_code(title)} ({unit}, {n_rounds} rounds)</summary>", "", "```text"]
            out += bar_chart(data, vers, unit, d) + [""] + box_chart(data, vers, unit, d)
            out += ["```", "",
                    "| version | n | min | p5 | median | mean | p95 | max | stdev | MAD | new vs this: Δ median [95% CI] |",
                    "|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---|"]
            for _, l, t, n in vers:
                s_ = summarize(data[n])
                pd = detail.get((title, n))
                dcell = f"{fmt_pct(pd[0])} [{fmt_pct(pd[1])}, {fmt_pct(pd[2])}]" if pd else ""
                name = f"**{t}** (new)" if n == new[3] else f"{l} {t}"
                out.append(f"| {name} | {s_['n']} | " + " | ".join(num(s_[k], d) for k in ("min", "p5", "median", "mean", "p95", "max", "stdev", "mad")) + f" | {dcell} |")
            out += ["", "</details>", ""]

    # --- pass rates (only with LLM suites)
    for key, title in (("tasks", "Agentic tasks pass rate"), ("prompts", "Prompt cases pass rate")):
        if key in res:
            tot = {v[3]: [0, 0] for v in vers}
            for e in res[key].values():
                for n, runs in e["tools"].items():
                    if n in tot:
                        tot[n][0] += sum(r["ok"] for r in runs)
                        tot[n][1] += len(runs)
            out += [f"#### {title} (higher is better)", "", "| version | passed |", "|---|---:|"]
            out += [f"| {l} {t} | {tot[n][0]}/{tot[n][1]} |" for _, l, t, n in vers] + [""]

    # --- caveats only (no method section)
    la = env.get("loadavg_start"), env.get("loadavg_end")
    warn = []
    if la[0] and env.get("cores") and la[0][0] > env["cores"] * 0.25:
        warn.append(f"system load was {la[0][0]:.2f} on {env['cores']} cores at start")
    if env.get("governor") and env["governor"] != "performance":
        warn.append(f"CPU governor is `{env['governor']}` (frequency scaling adds timing jitter, which the paired design cancels but does not remove)")
    if warn:
        out += ["", "**Caveats:** " + "; ".join(warn) + "."]
    out += ["", "</details>"]
    print("\n".join(out))


if __name__ == "__main__":
    main()
