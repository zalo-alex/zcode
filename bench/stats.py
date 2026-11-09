"""Small stdlib stats helpers shared by bench.py and the release report."""
import random
import statistics as st


def pct(xs, p):
    s = sorted(xs)
    k = (len(s) - 1) * p / 100
    lo, hi = int(k), min(int(k) + 1, len(s) - 1)
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def summarize(xs):
    xs = [x for x in xs if x is not None]
    if not xs:
        return None
    med = st.median(xs)
    return {"n": len(xs), "min": min(xs), "p5": pct(xs, 5), "p25": pct(xs, 25), "median": med, "p75": pct(xs, 75),
            "p95": pct(xs, 95), "max": max(xs), "mean": st.fmean(xs), "stdev": st.pstdev(xs) if len(xs) > 1 else 0.0,
            "mad": st.median(abs(x - med) for x in xs)}


def paired_delta(base, new, iters=2000, seed=1):
    """% change of median(new) vs median(base), with a 95% bootstrap CI over *rounds*.

    base/new are samples taken in the same interleaved rounds, so round i of both shares the same system state;
    resampling rounds (not samples) keeps that pairing. Returns (pct, lo, hi) or None.
    """
    n = min(len(base), len(new))
    if n < 5:
        return None
    base, new = base[:n], new[:n]
    mb = st.median(base)
    if mb == 0:
        return None
    rng = random.Random(seed)
    out = []
    for _ in range(iters):
        idx = [rng.randrange(n) for _ in range(n)]
        b = st.median(base[i] for i in idx)
        if b:
            out.append((st.median(new[i] for i in idx) - b) / b * 100)
    out.sort()
    return (st.median(new) - mb) / mb * 100, out[int(len(out) * 0.025)], out[int(len(out) * 0.975) - 1]
