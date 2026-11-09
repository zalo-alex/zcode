#!/usr/bin/env python3
"""Resolve the 4 comparison versions for a release and download their binaries from GitHub Releases.

Usage: fetch_release_bins.py [NEW_TAG] --out DIR [--wait SECONDS]

NEW_TAG defaults to the highest v* tag. Baselines (older than NEW):
  patch : previous release in the same major.minor line   (0.4.2 -> 0.4.1)
  minor : latest release of the previous minor line       (0.4.2 -> 0.3.x latest)
  major : latest release of the previous major line       (2.1.0 -> 1.x latest)
Missing/duplicate baselines are skipped. Prints one `zcode-<label>=<path>` per line on stdout
(ready for `bench.py --bin`), progress on stderr.
"""
import argparse, hashlib, json, os, platform, re, subprocess, sys, tarfile, time, urllib.error, urllib.request
from pathlib import Path


def log(*a):
    print(*a, file=sys.stderr, flush=True)


def repo_slug() -> str:
    url = subprocess.check_output(["git", "remote", "get-url", "origin"], text=True).strip()
    m = re.search(r"github\.com[:/](.+?)(?:\.git)?$", url)
    if not m:
        sys.exit(f"origin is not a GitHub remote: {url}")
    return m.group(1)


def ver(tag):
    return tuple(int(x) for x in tag.lstrip("v").split("."))


def pick(tags, new):
    n = ver(new)
    older = [t for t in tags if ver(t) < n]
    out = {}
    same_minor = [t for t in older if ver(t)[:2] == n[:2]]
    if same_minor:
        out["patch"] = same_minor[-1]
    prev_minor = [t for t in older if ver(t)[0] == n[0] and ver(t)[1] < n[1]]
    if prev_minor:
        out["minor"] = prev_minor[-1]
    prev_major = [t for t in older if ver(t)[0] < n[0]]
    if prev_major:
        out["major"] = prev_major[-1]
    return out


def http(url, method="GET"):
    req = urllib.request.Request(url, method=method, headers={"User-Agent": "zcode-release-bench"})
    tok = os.environ.get("GITHUB_TOKEN") or os.environ.get("GH_TOKEN")
    if tok:
        req.add_header("Authorization", f"Bearer {tok}")
    return urllib.request.urlopen(req, timeout=60)


def download(repo, tag, arch, dest: Path, wait: int) -> Path:
    name = f"zcode-linux-{arch}.tar.gz"
    base = f"https://github.com/{repo}/releases/download/{tag}"
    deadline = time.time() + wait
    while True:
        try:
            data = http(f"{base}/{name}").read()
            break
        except urllib.error.HTTPError as e:
            if e.code == 404 and time.time() < deadline:
                log(f"{tag}: asset not published yet, retrying in 20s (GitHub Actions still running?)")
                time.sleep(20)
                continue
            sys.exit(f"{tag}: cannot download {base}/{name}: HTTP {e.code}")
    try:
        want = http(f"{base}/{name}.sha256").read().split()[0].decode()
        if hashlib.sha256(data).hexdigest() != want:
            sys.exit(f"{tag}: sha256 mismatch")
    except urllib.error.HTTPError:
        log(f"{tag}: no .sha256 asset, skipping verification")
    dest.mkdir(parents=True, exist_ok=True)
    tgz = dest / name
    tgz.write_bytes(data)
    with tarfile.open(tgz) as t:
        t.extract("zcode", dest, filter="data")
    (dest / "zcode").chmod(0o755)
    return dest / "zcode"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("new_tag", nargs="?")
    ap.add_argument("--out", required=True)
    ap.add_argument("--wait", type=int, default=900, help="seconds to wait for the new release's assets")
    a = ap.parse_args()

    subprocess.run(["git", "fetch", "--tags", "--quiet"], check=False)
    tags = sorted((t for t in subprocess.check_output(["git", "tag", "-l", "v*"], text=True).split()
                   if re.fullmatch(r"v\d+\.\d+\.\d+", t)), key=ver)
    if not tags:
        sys.exit("no v* tags")
    new = a.new_tag or tags[-1]
    if new not in tags:
        sys.exit(f"tag {new} not found")
    arch = {"x86_64": "x86_64", "amd64": "x86_64", "aarch64": "aarch64", "arm64": "aarch64"}[platform.machine()]
    repo = repo_slug()

    wanted = {"new": new, **pick(tags, new)}
    seen, plan = set(), {}
    for label, tag in wanted.items():  # drop duplicate tags
        if tag not in seen:
            seen.add(tag)
            plan[label] = tag
    log("plan:", json.dumps(plan))

    out = Path(a.out)
    for label, tag in plan.items():
        cached = out / tag / "zcode"
        if label != "new" and cached.is_file():  # released tags are immutable: reuse earlier, sha-verified download
            log(f"{tag}: cached")
            print(f"zcode-{label}-{tag}={cached}")
            continue
        path = download(repo, tag, arch, out / tag, a.wait if label == "new" else 0)
        print(f"zcode-{label}-{tag}={path}")


if __name__ == "__main__":
    main()
