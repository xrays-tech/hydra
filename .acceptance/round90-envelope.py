import json, os, subprocess, signal, time, urllib.error, urllib.request, threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
ROOT=os.getcwd(); DIR=f'{ROOT}/.acceptance/tenant-boundaries-test'
TOKEN='hydra-tenant-boundaries-admin-2026'; T1='tenant-boundaries-token-t1-2026'
UP=18800; ADMIN, DATA = 18801, 18802
class U(BaseHTTPRequestHandler):
    def do_POST(self):
        n=int(self.headers.get('Content-Length',0)); self.rfile.read(n) if n else None
        b=json.dumps({"allowed":True,"expires_in":300}).encode()
        self.send_response(200); self.send_header('Content-Type','application/json'); self.send_header('Content-Length',str(len(b))); self.end_headers(); self.wfile.write(b)
    def log_message(self,*a): pass
threading.Thread(target=ThreadingHTTPServer(('127.0.0.1',UP),U).serve_forever,daemon=True).start()
env=dict(os.environ); env.update({'HYDRA_ADMIN_TOKEN':TOKEN,'HYDRA_ADMIN_ADDR':f'127.0.0.1:{ADMIN}','HYDRA_LISTEN':f'127.0.0.1:{DATA}',
 'HYDRA_DB_URL':f'sqlite://{DIR}/env.db?mode=rwc','HYDRA_ENCRYPTION_KEY':'MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=','RUST_LOG':'warn'})
log=open(f'{DIR}/env.log','w'); p=subprocess.Popen([f'{ROOT}/target/debug/hydra'],env=env,stdout=log,stderr=subprocess.STDOUT)
def call(m,u,token=None,body=None,host=None):
    d=None if body is None else json.dumps(body).encode(); h={'Content-Type':'application/json'}
    if token: h['Authorization']=f'Bearer {token}'
    if host: h['Host']=host
    r=urllib.request.Request(u,data=d,method=m,headers=h)
    try:
        with urllib.request.urlopen(r,timeout=5) as x: return x.status, x.read().decode()
    except urllib.error.HTTPError as e: return e.code, e.read().decode()
    except Exception as e: return 0,str(e)
try:
    for _ in range(60):
        if call('GET',f'http://127.0.0.1:{ADMIN}/api/v1/health',TOKEN)[0]==200: break
        time.sleep(0.25)
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/providers',TOKEN,{"id":"p1","key":"p1","name":"P","endpoint":f"http://127.0.0.1:{UP}","weight":1,"created_at":"","updated_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/provider-models',TOKEN,{"id":"pm1","key":"echo","name":"E","provider_id":"p1","status":1,"created_at":"","updated_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/provider-keys',TOKEN,{"id":"pk1","provider_id":"p1","api_key":"sk-up","created_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/tenants',TOKEN,{"id":"t1","name":"T","domain":"env.local","auth_url":f"http://127.0.0.1:{UP}/auth","enabled":True,"access_token":T1,"cert_key":None,"cert_file":None,"created_at":"","updated_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/tenant-providers',TOKEN,{"id":"tp1","tenant_id":"t1","provider_id":"p1","created_at":"","updated_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/tenant-models',TOKEN,{"id":"tm1","tenant_id":"t1","model_key":"echo","created_at":"","updated_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/reload',TOKEN,{}); time.sleep(0.4)
    cases={
      'no api key': ('POST', None, {'model':'echo','messages':[]}),
      'unknown model': ('POST','sk-tenant-1',{'model':'nope','messages':[]}),
      'unknown host': ('POST','sk-tenant-1',{'model':'echo','messages':[]}),
    }
    for name,(m,key,body) in cases.items():
        host='env.local' if name!='unknown host' else 'nowhere.local'
        st,txt=call(m,f'http://127.0.0.1:{DATA}/v1/chat/completions',key,body,host)
        print(f"{name:16s} -> HTTP {st} {txt[:120]}")
    # suspended tenant
    call('PUT',f'http://127.0.0.1:{ADMIN}/api/v1/tenants/t1',TOKEN,{"id":"t1","name":"T","domain":"env.local","auth_url":f"http://127.0.0.1:{UP}/auth","enabled":False,"cert_key":None,"cert_file":None,"created_at":"","updated_at":""})
    call('POST',f'http://127.0.0.1:{ADMIN}/api/v1/reload',TOKEN,{}); time.sleep(0.4)
    st,txt=call('POST',f'http://127.0.0.1:{DATA}/v1/chat/completions','sk-tenant-1',{'model':'echo','messages':[]},'env.local')
    print(f"{'suspended':16s} -> HTTP {st} {txt[:120]}")
finally:
    p.send_signal(signal.SIGKILL); p.wait(timeout=10)
