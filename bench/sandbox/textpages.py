#!/usr/bin/env python3
"""Which crates' code is resident in an idle zcode REPL?  usage: textpages.py <unstripped binary> [zcode args]
Run as root (zcode is non-dumpable).  Reads /proc/<pid>/pagemap for the binary's executable mapping, then attributes every
resident 4 KiB page to the symbols (from `nm`) that start in it, grouped by crate prefix."""
import os, re, struct, subprocess, sys, time, collections

exe = os.path.realpath(sys.argv[1])
env = dict(os.environ, ZCODE_CONFIG_CONTENT='{"provider":{"f":{"options":{"baseURL":"http://127.0.0.1:1/v1"}}},"model":"f/m"}')
os.makedirs("/tmp/bh", exist_ok=True)
p = subprocess.Popen([exe] + sys.argv[2:], stdin=subprocess.PIPE, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env, cwd="/tmp/bh")
time.sleep(1.0)
pid = p.pid
maps = open(f"/proc/{pid}/maps").read().splitlines()
segs = [(int(m.split()[0].split("-")[0], 16), int(m.split()[0].split("-")[1], 16), m.split()[1], int(m.split()[2], 16))
        for m in maps if m.endswith(exe) or exe in m]
text = [s for s in segs if "x" in s[2]][0]
base = min(s[0] - s[3] for s in segs)  # load bias: first mapping has file offset 0
resident = []
with open(f"/proc/{pid}/pagemap", "rb") as pm:
    for addr in range(text[0], text[1], 4096):
        pm.seek(addr // 4096 * 8)
        (e,) = struct.unpack("Q", pm.read(8))
        if e >> 63 & 1:
            resident.append(addr - base)
p.kill()

syms = []
for l in subprocess.run(["nm", "-C", "-n", "--defined-only", exe], capture_output=True, text=True).stdout.splitlines():
    f = l.split(None, 2)
    if len(f) == 3 and f[1] in "tTwW":
        syms.append((int(f[0], 16), f[2]))
syms.sort()
import bisect
starts = [s[0] for s in syms]

def crate(name):
    name = re.sub(r"^[<[{(]+", "", name)
    name = re.sub(r"^(core::ptr::drop_in_place<)", "", name)
    m = re.match(r"([A-Za-z0-9_]+)", name)
    return m.group(1) if m else name[:12]

by = collections.Counter()
for off in resident:
    i = bisect.bisect_right(starts, off + 4095) - 1
    j = bisect.bisect_right(starts, off) - 1
    seen = set()
    for k in range(max(j, 0), i + 1):
        seen.add(crate(syms[k][1]))
    for c in seen:
        by[c] += 1
print(f"{os.path.basename(exe)}: resident text pages {len(resident)} ({len(resident) * 4} KiB) of {(text[1] - text[0]) // 4096}")
for c, n in by.most_common(14):
    print(f"  {c:28} {n:4} pages")
