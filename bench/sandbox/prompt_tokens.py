import http.server, json, subprocess, threading, os, sys
got = {}
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers['content-length']); got['b']=json.loads(self.rfile.read(n))
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.end_headers()
        self.wfile.write(b'data: {"choices":[{"delta":{"role":"assistant","content":"ok"}}]}\n\ndata: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\ndata: [DONE]\n\n')
s=http.server.HTTPServer(('127.0.0.1',0),H); port=s.server_address[1]
threading.Thread(target=s.serve_forever,daemon=True).start()
for name,b,extra in [("baseline",os.path.expanduser("~/zcode-base"),[]),("new (default)",os.path.expanduser("~/zt/release/zcode"),["--sandbox","none"]),("new (--max-workers 4, lead)",os.path.expanduser("~/zt/release/zcode"),["--sandbox","none","--max-workers","4"])]:
    cfg=json.dumps({"provider":{"f":{"options":{"baseURL":f"http://127.0.0.1:{port}/v1"}}},"model":"f/m"})
    os.makedirs("/tmp/tokwork",exist_ok=True)
    subprocess.run([b,"run","--auto"]+extra+["hi"],env={**os.environ,"OPENCODE_CONFIG_CONTENT":cfg,"HOME":"/tmp/tokhome"},cwd="/tmp/tokwork",capture_output=True)
    body=got['b']; sysm=body['messages'][0]['content']; tools=json.dumps(body['tools'],separators=(',',':'))
    # system prompt includes cwd/date: constant length across runs
    print(f"{name}: system {len(sysm)} chars, tools {len(tools)} chars ({len(body['tools'])} tools), ~{(len(sysm)+len(tools))//4} tokens fixed")
