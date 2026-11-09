#!/usr/bin/env python3
"""CPU cost of the search tools: usage: toolcpu.py BIN [BIN...]
A fake model asks for N grep + glob calls over a synthetic tree; prints the median wall time of a whole `zcode run`
(tier 0, so the sandbox is not part of it).  Used to check that size-optimised builds keep the hot loops fast."""
import http.server, json, os, shutil, statistics, subprocess, sys, threading, time, random

ROOT = "/tmp/tcpu"
shutil.rmtree(ROOT, ignore_errors=True)
os.makedirs(ROOT + "/work")
random.seed(1)
words = ["alpha", "beta", "gamma", "delta", "fn", "let", "match", "struct", "impl", "TODO", "unsafe", "async", "await", "pub"]
for d in range(60):
    os.makedirs(f"{ROOT}/work/src/m{d}", exist_ok=True)
    for f in range(40):
        with open(f"{ROOT}/work/src/m{d}/f{f}.rs", "w") as fh:
            for _ in range(400):
                fh.write(" ".join(random.choice(words) for _ in range(8)) + "\n")
N = 24


class H(http.server.BaseHTTPRequestHandler):
    def log_message(self, *a): pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers["content-length"])))
        na = sum(1 for m in body["messages"] if m["role"] == "assistant")
        self.send_response(200); self.send_header("content-type", "text/event-stream"); self.end_headers()
        if na >= N:
            self.wfile.write(b'data: {"choices":[{"delta":{"role":"assistant","content":"done"}}]}\n\ndata: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\ndata: [DONE]\n\n')
            return
        if na % 8 == 7:
            call = ("bash", {"command": "true"})
        elif na % 2 == 0:
            call = ("grep", {"pattern": os.environ.get("PAT", r"(qqq|zzz)\d+\s+(async|await)\s+\w+"), "path": "."})  # default never matches: a full scan
        else:
            call = ("glob", {"pattern": "**/f1*.rs"})
        # vary the pattern so nothing is cached
        if call[0] == "grep": call[1]["pattern"] += "|zz%d" % na
        ch = [{"choices": [{"delta": {"role": "assistant", "content": ""}}]},
              {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "c0", "type": "function", "function": {"name": call[0], "arguments": json.dumps(call[1])}}]}}]},
              {"choices": [{"delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 1, "completion_tokens": 1}}]
        for c in ch: self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")


class S(http.server.ThreadingHTTPServer): daemon_threads = True


srv = S(("127.0.0.1", 0), H)
port = srv.server_address[1]
threading.Thread(target=srv.serve_forever, daemon=True).start()
cfg = json.dumps({"provider": {"f": {"options": {"baseURL": f"http://127.0.0.1:{port}/v1"}}}, "model": "f/m"})
env = {**os.environ, "OPENCODE_CONFIG_CONTENT": cfg, "ZCODE_NO_DAEMON": "1"}
for b in sys.argv[1:]:
    ts = []
    for _ in range(7):
        sb = [] if "base" in b else ["--sandbox", "none"]
        t = time.time()
        p = subprocess.run([b, "run", "--auto", "--max-steps", "400", "--max-tokens", "100000000"] + sb + ["go"], env=env, cwd=ROOT + "/work", capture_output=True)
        ts.append(time.time() - t)
        assert p.returncode == 0, p.stderr.decode()[-300:]
    print(f"{os.path.relpath(b, os.path.expanduser('~')):32} median {statistics.median(ts) * 1000:7.0f} ms for {N} search calls (min {min(ts) * 1000:.0f})")
