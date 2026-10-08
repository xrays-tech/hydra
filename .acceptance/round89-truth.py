import json, os, shutil, signal, subprocess, time, urllib.error, urllib.request
ROOT=os.getcwd(); DIR=os.path.join(ROOT,'.acceptance','tenant-contract-test')
ADMIN, DATA = 18780, 18781
TOKEN='hydra-tenant-contract-admin-2026'; T1='tenant-contract-token-t1-2026'
def call(m, url, token=None, body=None, host='contract.local'):
    data=None if body is None else json.dumps(body).encode()
    h={'Content-Type':'application/json'}
    if token: h['Authorization']=f'Bearer {token}'
    if host: h['Host']=host
    req=urllib.request.Request(url,data=data,method=m,headers=h)
    try:
        with urllib.request.urlopen(req,timeout=10) as r: return r.status, r.read().decode()[:70]
    except urllib.error.HTTPError as e: return e.code, e.read().decode()[:70]
    except Exception as e: return 0, str(e)[:70]
env=dict(os.environ); env.update({'HYDRA_ADMIN_TOKEN':TOKEN,'HYDRA_ADMIN_ADDR':f'127.0.0.1:{ADMIN}',
  'HYDRA_LISTEN':f'127.0.0.1:{DATA}','HYDRA_DB_URL':f'sqlite://{DIR}/truth.db?mode=rwc',
  'HYDRA_ENCRYPTION_KEY':'MDEyMzQ1Njc4OWFiY2RlZjAxMjM0NTY3ODlhYmNkZWY=','RUST_LOG':'warn'})
log=open(f'{DIR}/truth.log','w'); p=subprocess.Popen([f'{ROOT}/target/debug/hydra'],env=env,stdout=log,stderr=subprocess.STDOUT)
try:
    for _ in range(60):
        if call('GET',f'http://127.0.0.1:{ADMIN}/api/v1/health',TOKEN)[0]==200: break
        time.sleep(0.25)
    base=f'http://127.0.0.1:{DATA}/tenant/t1/api/v1'
    paths=['/sub-tenants','/sub-tenants/NAME','/sub-tenant-routes','/sub-tenant-routes/ID','/whoami','/usage','/auth/cache/invalidate']
    print(f"{'path':28s} " + " ".join(f"{m:>5s}" for m in ('GET','POST','PUT','DELETE')))
    for path in paths:
        row=[]
        for m in ('GET','POST','PUT','DELETE'):
            body={'sub_tenant_id':'x','provider_id':'p1','enabled':True} if m in ('PUT','POST') and 'routes' in path else ({'key_prefix':'AB_'} if m=='PUT' else {})
            st,txt=call(m, base+path, T1, body if m in ('PUT','POST') else None)
            row.append(str(st))
        print(f"{path:28s} " + " ".join(f"{c:>5s}" for c in row))
finally:
    p.send_signal(signal.SIGKILL); p.wait(timeout=10)
