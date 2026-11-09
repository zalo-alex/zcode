import http.server, json, subprocess, threading, os, sys, time, shutil, statistics, glob
HOME=os.path.expanduser("~"); BIN=HOME+"/zt/release/zcode"
ROOT="/tmp/b2"; shutil.rmtree(ROOT,ignore_errors=True); os.makedirs(ROOT+"/work"); os.makedirs(ROOT+"/home/cache"); os.makedirs(ROOT+"/run")
# fixture rootfs
fx=ROOT+"/fx"; os.makedirs(fx+"/bin"); os.makedirs(fx+"/etc"); shutil.copy("/usr/bin/busybox",fx+"/bin/busybox")
for a in subprocess.run([fx+"/bin/busybox","--list"],capture_output=True,text=True).stdout.split():
    p=fx+"/bin/"+a
    if not os.path.exists(p): os.symlink("busybox",p)
ENV={**os.environ,"HOME":ROOT+"/home","XDG_CACHE_HOME":ROOT+"/home/cache","XDG_STATE_HOME":ROOT+"/home/state","XDG_DATA_HOME":ROOT+"/home/data","XDG_RUNTIME_DIR":ROOT+"/run"}
subprocess.run([BIN,"image","import",fx,"--name","bb"],env=ENV,check=True,capture_output=True)
MODE={"n":0,"bash":"true","workers":0,"profile":None,"sleep":20}
def who(body):
    s=body["messages"][0]["content"]
    if "You are worker `" in s: return s.split("You are worker `")[1].split("`")[0]
    return "lead"
class H(http.server.BaseHTTPRequestHandler):
    def log_message(self,*a): pass
    def do_POST(self):
        n=int(self.headers['content-length']); body=json.loads(self.rfile.read(n))
        w=who(body); na=sum(1 for m in body["messages"] if m["role"]=="assistant")
        if MODE["workers"]:
            if w=="lead":
                if na==0:
                    calls=[("spawn",{"name":f"w{i}","prompt":"sleep","profile":MODE["profile"]} if MODE["profile"] else {"name":f"w{i}","prompt":"sleep"}) for i in range(MODE["workers"])]
                else: calls=[("wait",{})]
            else:
                calls=[("bash",{"command":f"sleep {MODE['sleep']}"})] if na==0 else None
        else:
            calls=[("bash",{"command":MODE["bash"]})] if na<MODE["n"] else None
        self.send_response(200); self.send_header('content-type','text/event-stream'); self.end_headers()
        if calls is None:
            self.wfile.write(b'data: {"choices":[{"delta":{"role":"assistant","content":"done"}}]}\n\ndata: {"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}\n\ndata: [DONE]\n\n')
        else:
            ch=[{"choices":[{"delta":{"role":"assistant","content":""}}]}]
            for i,(nm,a) in enumerate(calls):
                ch.append({"choices":[{"delta":{"tool_calls":[{"index":i,"id":f"c{i}","type":"function","function":{"name":nm,"arguments":json.dumps(a)}}]}}]})
            ch.append({"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":1,"completion_tokens":1}})
            for c in ch: self.wfile.write(f"data: {json.dumps(c)}\n\n".encode())
            self.wfile.write(b"data: [DONE]\n\n")
class S(http.server.ThreadingHTTPServer): daemon_threads=True
srv=S(('127.0.0.1',0),H); port=srv.server_address[1]; threading.Thread(target=srv.serve_forever,daemon=True).start()
cfg=json.dumps({"provider":{"f":{"options":{"baseURL":f"http://127.0.0.1:{port}/v1"}}},"model":"f/m","container":{"image":"bb"},"profiles":{"box":{"sandbox":"container"}}})
E={**ENV,"OPENCODE_CONFIG_CONTENT":cfg}
def run(extra,timeout=120): 
    t=time.time(); p=subprocess.run([BIN,"run","--auto","--max-steps","400","--max-tokens","100000000"]+extra+["go"],env=E,cwd=ROOT+"/work",capture_output=True,timeout=timeout); return time.time()-t,p
tiers=[("tier 0",["--sandbox","none"]),("tier 1",["--sandbox","landlock"]),("tier 2",["--sandbox","container"])]
print("### container create-from-scratch + per-command overhead (wall time of a whole `zcode run`)")
base={}
for name,fl in tiers:
    MODE.update(n=1,workers=0,bash="true"); t1=[]
    for _ in range(8): t,p=run(fl); t1.append(t)
    MODE.update(n=101); t101=[]
    for _ in range(3): t,p=run(fl); t101.append(t)
    one=statistics.median(t1)*1000; hund=statistics.median(t101)*1000
    base[name]=(one,hund)
    print(f"{name}: run with 1 command {one:.0f} ms; with 101 commands {hund:.0f} ms => {(hund-one)/100:.2f} ms per extra command (incl. fake-LLM round trip)")
o0=base["tier 0"]
for name in ["tier 1","tier 2"]:
    print(f"{name} vs tier 0: setup/first-command overhead {base[name][0]-o0[0]:+.0f} ms, per-command overhead {((base[name][1]-base[name][0])-(o0[1]-o0[0]))/100:+.2f} ms")
# RSS per agent
def pss(pid):
    try:
        for l in open(f"/proc/{pid}/smaps_rollup"):
            if l.startswith("Pss:"): return int(l.split()[1])
    except Exception: return 0
    return 0
def sample(workers,profile):
    MODE.update(workers=workers,profile=profile,sleep=25)
    p=subprocess.Popen([BIN,"run","--auto","--max-workers",str(workers+1),"--max-steps","400"]+(["--sandbox","none"] if profile is None else ["--sandbox","landlock"])+["go"],env=E,cwd=ROOT+"/work",stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
    time.sleep(4+0.5*workers)
    z=int(subprocess.run(["ps","-o","rss=","-p",str(p.pid)],capture_output=True,text=True).stdout.strip() or 0)
    kids=0
    out=subprocess.run(["ps","-eo","pid,args"],capture_output=True,text=True).stdout
    for l in out.splitlines()[1:]:
        pid,_,args=l.strip().partition(" ")
        if ("zcode-guest" in args or "zcode __ns" in args or "__ns {" in args) and ROOT in args or ("/zcode-guest" in args):
            kids+=pss(int(pid))
    p.kill(); p.wait(); time.sleep(1.5)
    return z,kids
print("### memory (Pss, kB) with N workers sleeping in a command")
res={}
for label,prof in [("in-process workers (tier 0)",None),("container workers (tier 2)","box")]:
    for n in (1,4,8):
        z,k=sample(n,prof); res[(label,n)]=(z,k)
        print(f"{label}, {n} worker(s): zcode {z} kB + container processes {k} kB = {z+k} kB")
    a=res[(label,1)]; b=res[(label,8)]
    print(f"  => per extra agent ~ {((b[0]+b[1])-(a[0]+a[1]))/7:.0f} kB")
