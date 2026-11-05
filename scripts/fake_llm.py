#!/usr/bin/env python3
"""Tiny fake OpenAI-compatible streaming server for e2e scripts (stdlib only).

usage: fake_llm.py PORT LOGFILE
Echoes "FAKE-REPLY: <first 60 chars of the last user message>".
Special words in the last user message: `slow` (3 s delay), `fail429` (HTTP 429 with 'rate limit exceeded'),
`usetool` (answers with an `ls` tool call first).
Every request body is appended to LOGFILE as one JSON line.
"""
import json
import sys
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

PORT, LOG = int(sys.argv[1]), sys.argv[2]


class H(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *a):
        pass

    def do_POST(self):
        body = json.loads(self.rfile.read(int(self.headers.get("content-length", 0))) or b"{}")
        with open(LOG, "a") as f:
            f.write(json.dumps(body) + "\n")
        users = [m for m in body.get("messages", []) if m.get("role") == "user"]
        text = users[-1]["content"] if users else ""
        if "fail429" in text:
            b = b"rate limit exceeded"
            self.send_response(429)
            self.send_header("content-length", str(len(b)))
            self.send_header("connection", "close")
            self.end_headers()
            self.wfile.write(b)
            return
        if body.get("messages", [{}])[-1].get("role") == "user" and "usetool" in text:
            # ask for one tool call (ls), then answer in text once the tool result is back
            self.send_response(200)
            self.send_header("content-type", "text/event-stream")
            self.send_header("connection", "close")
            self.end_headers()
            for c in [
                {"choices": [{"delta": {"role": "assistant", "content": "Looking."}}]},
                {"choices": [{"delta": {"tool_calls": [{"index": 0, "id": "call_1", "type": "function",
                                                        "function": {"name": "ls", "arguments": "{}"}}]}}]},
                {"choices": [{"delta": {}, "finish_reason": "tool_calls"}], "usage": {"prompt_tokens": 50, "completion_tokens": 5}},
            ]:
                self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
            self.close_connection = True
            return
        if "slow" in text:
            time.sleep(3)
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("connection", "close")
        self.end_headers()
        chunks = [
            {"choices": [{"delta": {"role": "assistant", "content": ""}}]},
            {"choices": [{"delta": {"content": "FAKE-REPLY: " + text[:60].replace("\n", " ")}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 50, "completion_tokens": 5}},
        ]
        for c in chunks:
            self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
        self.wfile.write(b"data: [DONE]\n\n")
        self.close_connection = True


ThreadingHTTPServer(("127.0.0.1", PORT), H).serve_forever()
