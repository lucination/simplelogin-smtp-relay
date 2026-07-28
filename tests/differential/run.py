#!/usr/bin/env python3
"""Live differential parity harness.

It deliberately clones/runs the upstream Python implementation instead of
reimplementing it, then runs each scenario through that process and the Rust
binary against identical in-process mock SimpleLogin and SMTP services.
"""
import base64, json, os, shutil, smtplib, socket, socketserver, subprocess, sys, tempfile, threading, time
from email.parser import BytesParser
from email.policy import SMTP
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

ROOT=Path(__file__).resolve().parents[2]
ORIG=ROOT / ".differential-original"
FIX={"alias@example.com": 7}
REVERSE={"a@example.com":"rev-a@simplelogin.test","b@example.com":"rev-b@simplelogin.test","c@example.com":"rev-c@simplelogin.test"}

class API(BaseHTTPRequestHandler):
 def log_message(self,*x): pass
 def _json(self,x,code=200): self.send_response(code);self.send_header("Content-Type","application/json");self.end_headers();self.wfile.write(json.dumps(x).encode())
 def do_GET(self):
  from urllib.parse import urlparse,parse_qs
  u=urlparse(self.path); q=parse_qs(u.query)
  if u.path!="/api/v2/aliases": return self._json({},404)
  # exact response enough to exercise GET, exact match, cache; special
  # missing sender exposes Alias not found failure.
  query=q.get("query",[""])[0]; page=int(q.get("page_id",["0"])[0])
  self._json({"aliases":[{"id":7,"email":query}]} if query in FIX and page==0 else {"aliases":[]})
 def do_POST(self):
  n=int(self.headers.get("Content-Length","0")); body=json.loads(self.rfile.read(n) or b"{}")
  if self.path!="/api/aliases/7/contacts": return self._json({},404)
  recipient=body.get("contact")
  if recipient in REVERSE: self._json({"reverse_alias":REVERSE[recipient]})
  elif recipient=="no-reverse@example.com": self._json({})
  else: self._json({"reverse_alias":"unknown@simplelogin.test"})

class SMTPHandler(socketserver.StreamRequestHandler):
 messages=[]; delay_data=False
 def put(self,s): self.wfile.write((s+"\r\n").encode());self.wfile.flush()
 def handle(self):
  self.put("220 mock upstream")
  sender=None;rcpts=[]
  while True:
   b=self.rfile.readline()
   if not b:return
   line=b.decode(errors="replace").rstrip("\r\n"); up=line.upper()
   if up.startswith("EHLO") or up.startswith("HELO"): self.put("250-mock");self.put("250-AUTH LOGIN PLAIN");self.put("250 OK")
   elif up.startswith("AUTH PLAIN"): self.put("235 OK")
   elif up=="AUTH LOGIN" or up.startswith("AUTH LOGIN "):
    if up=="AUTH LOGIN":
     self.put("334 VXNlcm5hbWU6");self.rfile.readline()
    self.put("334 UGFzc3dvcmQ6");self.rfile.readline();self.put("235 OK")
   elif up.startswith("MAIL FROM:"): sender=line.split(":",1)[1].strip("<>");self.put("250 OK")
   elif up.startswith("RCPT TO:"): rcpts.append(line.split(":",1)[1].strip("<>"));self.put("250 OK")
   elif up=="DATA":
    self.put("354 go")
    data=b""
    while True:
     x=self.rfile.readline()
     if x in (b".\r\n",b".\n",b""):break
     if x.startswith(b".."):x=x[1:]
     data+=x
    if self.delay_data: time.sleep(2)
    self.messages.append((sender,rcpts,data));self.put("250 OK")
   elif up=="QUIT":self.put("221 Bye");return
   elif up=="STARTTLS":self.put("454 TLS unavailable")
   else:self.put("500 bad")

def free_port():
 s=socket.socket();s.bind(("127.0.0.1",0));p=s.getsockname()[1];s.close();return p

def wait(port):
 for _ in range(100):
  try:
   with socket.create_connection(("127.0.0.1",port),.1):return
  except OSError:time.sleep(.05)
 raise RuntimeError("server did not start")
def start(cls,port):
 s=socketserver.ThreadingTCPServer(("127.0.0.1",port),cls);s.daemon_threads=True;threading.Thread(target=s.serve_forever,daemon=True).start();return s

def env(relay,api,upstream,timeout=3):
 e=os.environ.copy();e.update({"RELAY_HOST":"127.0.0.1","RELAY_PORT":str(relay),"RELAY_USERNAME":"relay","RELAY_PASSWORD":"secret","SL_API_URL":api,"SL_API_KEY":"test-key","UPSTREAM_HOST":"127.0.0.1","UPSTREAM_PORT":str(upstream),"UPSTREAM_USERNAME":"up","UPSTREAM_PASSWORD":"up-pass","UPSTREAM_STARTTLS":"false","DATA_TIMEOUT":str(timeout),"UPSTREAM_TIMEOUT":"2","LOG_LEVEL":"ERROR"});return e

def start_relay(kind,relay,api,up,timeout=3):
 e=env(relay,api,up,timeout)
 if kind=="python": cmd=[sys.executable,"server.py"];cwd=ORIG
 else: cmd=[str(ROOT/"target"/"debug"/"smtp-relay")];cwd=ROOT
 p=subprocess.Popen(cmd,cwd=cwd,env=e,stdout=subprocess.PIPE,stderr=subprocess.PIPE)
 try:
  wait(relay)
 except Exception:
  p.terminate(); _,err=p.communicate(timeout=3)
  raise RuntimeError(f"{kind} relay did not start: {err.decode(errors='replace')}")
 return p

def smtp_case(port, mail_from, rcpts, message):
 try:
  c=smtplib.SMTP("127.0.0.1",port,timeout=8);c.ehlo();c.login("relay","secret");result=c.sendmail(mail_from,rcpts,message);c.quit();return (250,result)
 except smtplib.SMTPResponseException as e:return (e.smtp_code,e.smtp_error.decode(errors="replace"))
 finally:
  try:c.close()
  except:pass

def normalized(msg):
 m=BytesParser(policy=SMTP).parsebytes(msg);return {"to":m.get("To"),"cc":m.get("Cc"),"bcc":m.get("Bcc"),"subject":m.get("Subject")}
def run(kind,scenario,api,up):
 SMTPHandler.messages=[];port=free_port();p=start_relay(kind,port,api,up,scenario.get("timeout",3))
 try:
  response=smtp_case(port,scenario["from"],scenario["rcpts"],scenario["msg"])
  time.sleep(0.15)
  sent=[(a,b,normalized(c)) for a,b,c in SMTPHandler.messages]
  return response,sent
 finally:p.terminate();p.wait(timeout=5)
def main():
 if not ORIG.exists():
  subprocess.run(["git","clone","--depth","1","https://github.com/Hoshinowo-Yuki/simple-login-smtp-relay.git",str(ORIG)],check=True,stdout=subprocess.DEVNULL)
 api_port,up_port=free_port(),free_port();api=start(API,api_port);up=start(SMTPHandler,up_port);api_url=f"http://127.0.0.1:{api_port}"
 scenarios=[
  ("plain_to",{"from":"alias@example.com","rcpts":["a@example.com"],"msg":b"From: alias@example.com\r\nTo: a@example.com\r\nSubject: one\r\n\r\nbody\r\n"}),
  ("multiple_to",{"from":"alias@example.com","rcpts":["a@example.com","b@example.com"],"msg":b"From: alias@example.com\r\nTo: a@example.com, b@example.com\r\nSubject: two\r\n\r\nbody\r\n"}),
  ("to_cc_display_bcc",{"from":"alias@example.com","rcpts":["a@example.com","b@example.com","c@example.com"],"msg":b"From: alias@example.com\r\nTo: Alice A <a@example.com>, unknown@example.com\r\nCc: Bee <b@example.com>, c@example.com\r\nBcc: hidden@example.com\r\nSubject: three\r\n\r\nbody\r\n"}),
  ("alias_not_found",{"from":"missing@example.com","rcpts":["a@example.com"],"msg":b"To: a@example.com\r\nSubject: bad\r\n\r\nx\r\n"}),
  ("no_reverse_alias",{"from":"alias@example.com","rcpts":["no-reverse@example.com"],"msg":b"To: no-reverse@example.com\r\n\r\nx\r\n"}),
 ]
 passed=0
 for name,s in scenarios:
  py=run("python",s,api_url,up_port);rs=run("rust",s,api_url,up_port)
  assert py==rs,(name,py,rs)
  print(f"PASS {name}: response={py[0][0]} relays={len(py[1])}");passed+=1
 # Timeout: the Python original's handle_DATA wraps a fully synchronous
 # _process() (blocking requests/smtplib calls, no `await` inside) in
 # asyncio.wait_for(). Because the coroutine never yields control back to the
 # event loop mid-flight, wait_for can NEVER actually preempt it: DATA_TIMEOUT
 # only produces "451 Timeout processing mail" if the blocking call somehow
 # returns exactly as/after the deadline check races it, which does not
 # happen for a slow-but-completing upstream. This is a genuine upstream bug,
 # not something worth reproducing: our Rust port runs the blocking work in
 # spawn_blocking behind a real tokio::time::timeout, so DATA_TIMEOUT is
 # actually enforced. See KNOWN_DIFFERENCES.md. We assert the ACTUAL
 # (divergent) behavior of each implementation here rather than pretend they
 # agree.
 SMTPHandler.delay_data=True;s={"from":"alias@example.com","rcpts":["a@example.com"],"msg":b"To: a@example.com\r\n\r\ntimeout\r\n","timeout":1}
 py=run("python",s,api_url,up_port);rs=run("rust",s,api_url,up_port);SMTPHandler.delay_data=False
 assert py[0][0]==250,("python original did not relay through the slow-upstream case as expected",py)
 assert rs[0][0]==451,("rust DATA_TIMEOUT did not fire as expected",rs)
 print("PASS upstream_timeout_via_DATA_TIMEOUT (documented divergence, see KNOWN_DIFFERENCES.md): python=250 (asyncio.wait_for can't preempt sync blocking I/O) rust=451 (real timeout enforced)");passed+=1
 print(f"Differential parity PASS: {passed}/{passed} scenarios")
 api.shutdown();up.shutdown()
if __name__=="__main__":main()
