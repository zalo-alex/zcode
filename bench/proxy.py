"""Metering proxy: sits between an agent CLI and the real OpenAI-compatible endpoint.

Records per LLM request: body size, message/tool counts, system prompt size, tool-schema size,
TTFT, total latency, token usage (from SSE `usage`, else chars/4 estimate).
"""
import http.client
import json
import re
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlparse


# first real generated token (content / reasoning / tool call), not the initial role-only chunk
_FIRST_TOKEN = re.compile(rb'"(?:content|reasoning_content|reasoning)"\s*:\s*"[^"]|"tool_calls"\s*:\s*\[\s*\{')


class Meter:
    def __init__(self, upstream: str):
        u = urlparse(upstream)
        self.host, self.port, self.prefix = u.hostname, u.port or 80, u.path.rstrip("/")
        self.records: list[dict] = []
        self.lock = threading.Lock()
        self.server: ThreadingHTTPServer | None = None
        self.port_local = 0

    def reset(self):
        with self.lock:
            self.records = []

    def snapshot(self) -> list[dict]:
        with self.lock:
            return list(self.records)

    def start(self) -> str:
        meter = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def _fwd(self, method):
                n = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(n) if n else b""
                path = self.path
                if path.startswith("/v1"):
                    path = path[3:]
                t0 = time.perf_counter()
                c = http.client.HTTPConnection(meter.host, meter.port, timeout=600)
                hdrs = {k: v for k, v in self.headers.items() if k.lower() not in ("host", "content-length", "connection")}
                hdrs["Content-Length"] = str(len(body))
                c.request(method, meter.prefix + path, body=body, headers=hdrs)
                r = c.getresponse()
                self.send_response(r.status)
                for k, v in r.getheaders():
                    if k.lower() not in ("transfer-encoding", "connection", "content-length"):
                        self.send_header(k, v)
                self.send_header("Transfer-Encoding", "chunked")
                self.end_headers()
                rec = None
                if method == "POST" and "chat/completions" in path:
                    rec = meter._request_info(body)
                ttft, buf, tail = None, b"", b""
                while True:
                    chunk = r.read1(8192) if hasattr(r, "read1") else r.read(8192)
                    if not chunk:
                        break
                    if ttft is None and _FIRST_TOKEN.search(chunk):
                        ttft = time.perf_counter() - t0
                    tail = (tail + chunk)[-4096:] if rec else b""
                    buf += chunk if rec and len(buf) < 2_000_000 else b""
                    try:
                        self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
                        self.wfile.flush()
                    except OSError:
                        break
                try:
                    self.wfile.write(b"0\r\n\r\n")
                    self.wfile.flush()
                except OSError:
                    pass
                c.close()
                if rec is not None:
                    total = time.perf_counter() - t0
                    rec.update(meter._response_info(buf, r.status))
                    rec.update({"ttft": ttft if ttft is not None else total, "latency": total, "status": r.status})
                    with meter.lock:
                        meter.records.append(rec)

            def do_POST(self):
                self._fwd("POST")

            def do_GET(self):
                self._fwd("GET")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.server.daemon_threads = True
        self.port_local = self.server.server_address[1]
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.port_local}/v1"

    @staticmethod
    def _request_info(body: bytes) -> dict:
        try:
            j = json.loads(body)
        except Exception:
            return {"req_bytes": len(body)}
        msgs = j.get("messages", [])
        sys_chars = sum(len(m.get("content") or "") if isinstance(m.get("content"), str) else len(json.dumps(m.get("content")))
                        for m in msgs if m.get("role") in ("system", "developer"))
        tools = j.get("tools") or []
        return {"req_bytes": len(body), "n_messages": len(msgs), "sys_chars": sys_chars,
                "n_tools": len(tools), "tools_chars": len(json.dumps(tools)) if tools else 0,
                "est_prompt_tokens": len(body) // 4}

    @staticmethod
    def _response_info(buf: bytes, status: int) -> dict:
        out = {"prompt_tokens": None, "completion_tokens": None, "out_chars": 0, "tool_calls": 0}
        for line in buf.decode("utf-8", "replace").splitlines():
            if not line.startswith("data:") or line.endswith("[DONE]"):
                continue
            try:
                j = json.loads(line[5:])
            except Exception:
                continue
            u = j.get("usage")
            if u:
                out["prompt_tokens"] = u.get("prompt_tokens")
                out["completion_tokens"] = u.get("completion_tokens")
            for ch in j.get("choices") or []:
                d = ch.get("delta") or {}
                out["out_chars"] += len(d.get("content") or "") + len(d.get("reasoning_content") or "")
                if d.get("tool_calls"):
                    out["tool_calls"] += sum(1 for t in d["tool_calls"] if (t.get("function") or {}).get("name"))
        return out
