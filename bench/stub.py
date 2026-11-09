"""Instant fake OpenAI-compatible endpoint (SSE). No network, no billing, deterministic.

Used by the `local` suite so we measure the client (spawn, request build, stream parsing, rendering) and not
upstream latency. Every chat/completions POST is answered with `reply_chars` of text split into small deltas.
Records arrival time of the first request (for time-to-first-request) and request shape info.
"""
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

from proxy import Meter


class Stub:
    def __init__(self):
        self.reply = "PONG"
        self.delta = 4  # chars per SSE chunk
        self.requests: list[dict] = []
        self.lock = threading.Lock()
        self.server = None

    def set_reply(self, text: str):
        self.reply = text

    def reset(self):
        with self.lock:
            self.requests = []

    def start(self) -> str:
        stub = self

        class H(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *a):
                pass

            def do_GET(self):
                body = json.dumps({"data": [{"id": "stub"}]}).encode()
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(body)))
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                t = time.perf_counter()
                n = int(self.headers.get("content-length") or 0)
                body = self.rfile.read(n) if n else b""
                info = Meter._request_info(body)
                info["t"] = t
                with stub.lock:
                    stub.requests.append(info)
                text, d = stub.reply, stub.delta
                chunks = [text[i:i + d] for i in range(0, len(text), d)] or [""]
                out = []
                for i, c in enumerate(chunks):
                    delta = {"content": c}
                    if i == 0:
                        delta["role"] = "assistant"
                    out.append(f"data: {json.dumps({'id': 'x', 'object': 'chat.completion.chunk', 'model': 'stub', 'choices': [{'index': 0, 'delta': delta, 'finish_reason': None}]})}\n\n")
                out.append(f"data: {json.dumps({'id': 'x', 'object': 'chat.completion.chunk', 'model': 'stub', 'choices': [{'index': 0, 'delta': {}, 'finish_reason': 'stop'}], 'usage': {'prompt_tokens': max(1, info.get('req_bytes', 4) // 4), 'completion_tokens': max(1, len(text) // 4), 'total_tokens': 0}})}\n\n")
                out.append("data: [DONE]\n\n")
                payload = "".join(out).encode()
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                try:
                    self.wfile.write(payload)
                except OSError:
                    pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.server.daemon_threads = True
        threading.Thread(target=self.server.serve_forever, daemon=True).start()
        return f"http://127.0.0.1:{self.server.server_address[1]}/v1"

    def stop(self):
        if self.server:
            self.server.shutdown()
            self.server.server_close()
