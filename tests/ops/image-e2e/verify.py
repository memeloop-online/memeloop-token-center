import hashlib,json,time,urllib.request,urllib.error
BASE='http://127.0.0.1:18080'; SERVICE='synthetic-service-token-0123456789abcdef'; TENANT='image-e2e-synthetic'; MODEL='image-e2e-model'; SESSION='image-e2e-session'
def request(path,method='GET',data=None,token=SERVICE):
    req=urllib.request.Request(BASE+path,method=method,data=None if data is None else json.dumps(data).encode(),headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=40) as response:return response.status,response.headers,response.read()
def post(path,data): return json.loads(request(path,'POST',data)[2])
u=post('/internal/v1/upstreams',{'tenant_external_id':TENANT,'name':MODEL,'driver':'http-json','config':{'base_url':'http://127.0.0.1:18082/scenario/normal','network_scope':'private'},'credential':{'type':'none'}})
r=post('/internal/v1/model-routes',{'tenant_external_id':TENANT,'public_model':MODEL,'upstream_account_id':u['id'],'upstream_model':MODEL,'protocol':'openai','priority':0,'custom_model_confirmed':True})
post('/internal/v1/prices/USD/'+MODEL,{'input_per_million':'0.01','output_per_million':'0.02'})
k=post('/internal/v1/keys',{'tenant_external_id':TENANT,'principal_external_id':TENANT,'alias':'isolated image e2e','currency':'USD','initial_balance':'100','route_ids':[r['id']],'policy':{'requests_per_minute':60,'tokens_per_minute':100000,'max_concurrency':1,'enforcement_mode':'metered_unlimited','daily_budget':None,'weekly_budget':None,'lifetime_budget':None}})['key']
req=urllib.request.Request(BASE+'/v1/chat/completions',data=json.dumps({'model':MODEL,'messages':[{'role':'user','content':'synthetic image e2e'}],'stream':True,'max_tokens':1,'benchmark':{'mode':'stream','bytes':4096,'chunk_bytes':4096,'delay_ms':0}}).encode(),headers={'Authorization':'Bearer '+k,'Content-Type':'application/json','x-mtc-conversation-id':SESSION},method='POST')
with urllib.request.urlopen(req,timeout=60) as x:
 rid=x.headers.get('x-mtc-request-id'); body=x.read(); assert x.status==200 and rid and body.count(b'data: [DONE]')==1
print(json.dumps({'request_id':rid,'stream_status':200,'stream_bytes':len(body),'stream_sha256':hashlib.sha256(body).hexdigest(),'terminal':True}))
for i in range(90):
 try:
  _,_,raw=request('/self/v1/requests/'+rid,token=k);d=json.loads(raw)
  if d.get('archive_complete') and d.get('completed_at') and (d.get('usage') or {}).get('tokens'):break
 except urllib.error.HTTPError:pass
 time.sleep(2)
else:raise RuntimeError('request terminal, usage and archive did not converge')
assert d['request_id']==rid
assert d['usage']['tokens']['input_tokens']==2 and d['usage']['tokens']['output_tokens']==1
for side in ('request','response'):
 status,_,raw=request('/self/v1/requests/'+rid+'/archive/'+side,token=k);assert status==200 and raw
 print(json.dumps({'request_id':rid,'side':side,'status':status,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()}))
status,_,raw=request('/self/v1/sessions/'+SESSION,token=k);session=json.loads(raw);assert status==200 and rid in json.dumps(session)
print(json.dumps({'request_id':rid,'session_status':status,'usage':d['usage'],'archive_complete':True}))
