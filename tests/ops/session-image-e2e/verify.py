import hashlib,json,time,urllib.request,urllib.error,os,uuid,urllib.parse
BASE='http://127.0.0.1:18080'; SERVICE=os.environ['E2E_SERVICE_TOKEN']; NONCE=uuid.uuid4().hex; TENANT='image-e2e-'+NONCE; MODEL=TENANT+'-model'; SESSION=TENANT+'-session'
def request(path,method='GET',data=None,token=SERVICE):
    req=urllib.request.Request(BASE+path,method=method,data=None if data is None else json.dumps(data).encode(),headers={'Authorization':'Bearer '+token,'Content-Type':'application/json'})
    with urllib.request.urlopen(req,timeout=40) as response:return response.status,response.headers,response.read()
def post(path,data): return json.loads(request(path,'POST',data)[2])
u=post('/internal/v1/upstreams',{'tenant_external_id':TENANT,'name':MODEL,'driver':'http-json','config':{'base_url':'http://127.0.0.1:18082/scenario/normal','network_scope':'private'},'credential':{'type':'none'}})
r=post('/internal/v1/model-routes',{'tenant_external_id':TENANT,'public_model':MODEL,'upstream_account_id':u['id'],'upstream_model':MODEL,'protocol':'openai','priority':0,'custom_model_confirmed':True})
post('/internal/v1/prices/USD/'+MODEL,{'input_per_million':'0.01','output_per_million':'0.02'})
k=post('/internal/v1/keys',{'tenant_external_id':TENANT,'principal_external_id':TENANT,'alias':'isolated image e2e','currency':'USD','initial_balance':'100','route_ids':[r['id']],'policy':{'requests_per_minute':60,'tokens_per_minute':100000,'max_concurrency':1,'enforcement_mode':'metered_unlimited','daily_budget':None,'weekly_budget':None,'lifetime_budget':None}})['key']
messages=[{'role':'user','content':'synthetic image e2e '+NONCE}]+[{'role':'user','content':f'page-marker-{NONCE}-{i}'} for i in range(1,67)]
messages[1]['content']='large-body-'+NONCE+'😀'*40000
messages[33]={'role':'user','content':[{'type':'image_url','image_url':{'url':'data:image/png;base64,c3ludGhldGljLW1lZGlh'}}]}
mode=os.environ.get('E2E_FAULT_MODE','normal')
if mode!='normal': urllib.request.urlopen('http://127.0.0.1:19001/__test__/'+mode).read()
req=urllib.request.Request(BASE+'/v1/chat/completions',data=json.dumps({'model':MODEL,'messages':messages,'stream':True,'max_tokens':1,'benchmark':{'mode':'stream','bytes':4096,'chunk_bytes':4096,'delay_ms':0}}).encode(),headers={'Authorization':'Bearer '+k,'Content-Type':'application/json','x-mtc-conversation-id':SESSION},method='POST')
with urllib.request.urlopen(req,timeout=60) as x:
 rid=x.headers.get('x-mtc-request-id'); body=x.read(); assert x.status==200 and rid and body.count(b'data: [DONE]')==1
if mode!='normal':
 faults=int(urllib.request.urlopen('http://127.0.0.1:19001/__test__/count').read()); assert faults>0, 'fault mode did not encounter an object PUT'
 urllib.request.urlopen('http://127.0.0.1:19001/__test__/normal').read()
 print(json.dumps({'fault_mode':mode,'intercepted_puts':faults}))
frames=body.decode('utf-8').strip().split('\n\n')
assert frames[-1]=='data: [DONE]' and sum(frame=='data: [DONE]' for frame in frames)==1
parsed=[json.loads(frame[6:]) for frame in frames[:-1] if frame.startswith('data: ')]
assert len(parsed)==len(frames)-1
finish=[i for i,frame in enumerate(parsed) if any(c.get('finish_reason')=='stop' for c in frame.get('choices',[]))]
usage=[i for i,frame in enumerate(parsed) if frame.get('usage') is not None]
assert len(finish)==1 and len(usage)==1 and finish[0]<usage[0]==len(parsed)-1
assert parsed[usage[0]]['usage']['prompt_tokens']==2 and parsed[usage[0]]['usage']['completion_tokens']==1
assert any(c.get('delta',{}).get('content') for frame in parsed[:finish[0]] for c in frame.get('choices',[]))
marker=hashlib.sha256(('synthetic image e2e '+NONCE).encode()).hexdigest()
with open('/tmp/mtc-image-e2e-mock.jsonl') as evidence: observed=[json.loads(line) for line in evidence]
assert sum(row['marker_sha256']==marker for row in observed)==1
open('/tmp/session-e2e-request-id','w').write(rid)
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
archives={}
for side in ('request','response'):
 status,_,raw=request('/self/v1/requests/'+rid+'/archive/'+side,token=k);assert status==200 and raw; archives[side]=raw
 print(json.dumps({'request_id':rid,'side':side,'status':status,'bytes':len(raw),'sha256':hashlib.sha256(raw).hexdigest()}))
assert NONCE.encode() in archives['request']
assert archives['response']==body
for i in range(90):
 status,_,raw=request('/self/v1/sessions?limit=100',token=k); listing=json.loads(raw); matched=[]
 for entry in listing['sessions']:
  if entry.get('unlinked') or not entry.get('cluster_id'):continue
  try:
   status,_,raw=request('/self/v1/sessions/'+urllib.parse.quote(entry['session_id'],safe=''),token=k); session=json.loads(raw)
   if status==200 and any(x.get('request_id')==rid for x in session['requests']):matched.append(entry['session_id'])
  except urllib.error.HTTPError:pass
 if len(matched)==1:break
 time.sleep(2)
else:
 print(json.dumps({'session_list_count':len(listing['sessions']),'matching_request_count':len(matched),'request_id':rid,'worker_role':'all'}),flush=True)
 raise RuntimeError('same request did not appear in session projection')
print(json.dumps({'request_id':rid,'session_status':status,'usage':d['usage'],'archive_complete':True}))

# Verify the actual published semantic manifest through its signed cursor chain.
path='/self/v1/sessions/'+urllib.parse.quote(matched[0],safe='')+'/requests/'+rid+'/semantic-view'
last_status=None; last_state=None
for i in range(120):
 try:
  status,_,raw=request(path,token=k);page=json.loads(raw);last_status=status;last_state=page.get('status')
  if last_state=='published':break
 except urllib.error.HTTPError as error:last_status=error.code
 time.sleep(2)
else:
 print(json.dumps({'semantic_http':last_status,'semantic_state':last_state,'session_shape':matched[0][:9],'request_id':rid}),flush=True)
 raise RuntimeError('semantic manifest did not publish')
instances=[];pages=0
while True:
 pages+=1;instances.extend(page['instances']);cursor=page.get('next_cursor')
 if not cursor:break
 status,_,raw=request(path+'?'+urllib.parse.urlencode({'cursor':cursor}),token=k);page=json.loads(raw)
print(json.dumps({'semantic_pages':pages,'semantic_instances':len(instances),'request_id':rid}),flush=True)
assert pages>=2 and len(instances)>=67
assert len({item['instance_id'] for item in instances})==len(instances)
found_media=False;found_large=False
for item in instances:
 status,_,raw=request(path+'?'+urllib.parse.urlencode({'parts_cursor':item['atom_cursor']}),token=k);part_page=json.loads(raw)
 for part in part_page.get('parts',[]):
  media=(part.get('media') or {}).get('read_cursor')
  if media:
   status,_,raw=request(path+'?'+urllib.parse.urlencode({'media_cursor':media}),token=k);assert status==200 and raw==b'synthetic-media';found_media=True
  for field in part.get('fields',[]):
   cursor=field.get('content_cursor')
   if not cursor:continue
   status,_,raw=request(path+'?'+urllib.parse.urlencode({'content_cursor':cursor}),token=k)
   frames=[json.loads(line) for line in raw.splitlines() if line]
   text=''.join(frame.get('text','') for frame in frames)
   if text=='large-body-'+NONCE+'😀'*40000:
    assert frames and frames[-1].get('complete') is True;found_large=True
print(json.dumps({'media_found':found_media,'large_text_found':found_large}),flush=True)
assert found_media and found_large
other=post('/internal/v1/keys',{'tenant_external_id':'other-'+NONCE,'principal_external_id':'other-'+NONCE,'alias':'other tenant','currency':'USD','initial_balance':'100','route_ids':[],'policy':{'requests_per_minute':60,'tokens_per_minute':100000,'max_concurrency':1,'enforcement_mode':'metered_unlimited','daily_budget':None,'weekly_budget':None,'lifetime_budget':None}})['key']
try:request(path,token=other);raise RuntimeError('cross-tenant semantic read succeeded')
except urllib.error.HTTPError as e:assert e.code in (403,404)
print(json.dumps({'request_id':rid,'semantic_pages':pages,'instances':len(instances),'large_text_verified':found_large,'media_bytes':15,'cross_tenant_denied':True}))
