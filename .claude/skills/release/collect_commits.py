#!/usr/bin/env python3
"""Print the commits of a release as plain text for writing release notes.

Usage: collect_commits.py [TAG]
  TAG given  -> commits in (previous v* tag, TAG]
  no TAG     -> commits in (latest v* tag, HEAD]   (unreleased)
Per commit: short hash, subject, body (if any) and changed files with +/- counts, so vague subjects can be
understood. Header line gives the range and the compare URL. Merge commits are skipped.
"""
import re, subprocess, sys


def git(*a):
    return subprocess.check_output(["git", *a], text=True).strip()


def ver(t):
    return tuple(int(x) for x in t.lstrip("v").split("."))


def main():
    tags = sorted((t for t in git("tag", "-l", "v*").split() if re.fullmatch(r"v\d+\.\d+\.\d+", t)), key=ver)
    if len(sys.argv) > 1:
        new = sys.argv[1]
        if new not in tags:
            sys.exit(f"tag {new} not found")
        older = [t for t in tags if ver(t) < ver(new)]
        prev = older[-1] if older else None
    else:
        new, prev = "HEAD", (tags[-1] if tags else None)
    rng = f"{prev}..{new}" if prev else new
    url = re.search(r"github\.com[:/](.+?)(?:\.git)?$", git("remote", "get-url", "origin"))
    print(f"range: {rng}")
    if url and prev:
        print(f"compare: https://github.com/{url[1]}/compare/{prev}...{new if new != 'HEAD' else 'main'}")
    sep = "\x1e"
    log = git("log", "--no-merges", "--reverse", f"--format=%h{sep}%s{sep}%b{sep}%x00", rng)
    for rec in log.split("\x00"):
        rec = rec.strip()
        if not rec:
            continue
        h, subj, body, *_ = (rec.split(sep) + ["", ""])
        stat = git("show", "--no-color", "--format=", "--numstat", h).splitlines()
        files = [f"{p.split(chr(9))[2]} (+{p.split(chr(9))[0]}/-{p.split(chr(9))[1]})" for p in stat if p.count("\t") == 2]
        print(f"\n## {h} {subj}")
        if body.strip():
            print(body.strip())
        if files:
            print("files: " + ", ".join(files[:12]) + (f", … +{len(files) - 12} more" if len(files) > 12 else ""))


if __name__ == "__main__":
    main()
