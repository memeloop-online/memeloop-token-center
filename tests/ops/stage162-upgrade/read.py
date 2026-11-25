import hashlib,json,os,subprocess,urllib.request,urllib.error,urllib.parse
with open(os.environ['E2E_IDENTITY_FILE']) as f: data=json.load(f)
base=os.environ.get('E2E_BASE','http://127.0.0.1:18080');rid=data['request_id']
def get(path):
 req=urllib.request.Request(base+path,headers={'Authorization':'Bearer '+data['key']})
 with urllib.request.urlopen(req,timeout=40) as r:
  if r.status!=200: raise RuntimeError('unexpected HTTP status')
  return r.read()
def s3(key):
 if not key.startswith('tenants/') or '..' in key: raise RuntimeError('invalid object key')
 config='user = "rustfs-test-fixture:'+os.environ['E2E_S3_PASSWORD']+'"\n'
 return subprocess.run(['curl','-fsS','--aws-sigv4','aws:amz:us-east-1:s3','--config','-','http://127.0.0.1:19000/image-e2e/'+urllib.parse.quote(key,safe='/')],input=config.encode(),stdout=subprocess.PIPE,check=True).stdout
def dbtenant():
 env=dict(os.environ,PGPASSWORD=os.environ['E2E_PG_PASSWORD'])
 value=subprocess.check_output(['docker','exec','-e','PGPASSWORD','image-e2e-postgres','psql','-U','token_center','-d','token_center_test','-Atqc',"SELECT tenant_id FROM request_records WHERE id='"+rid+"'"],env=env,text=True).strip()
 import uuid
 return str(uuid.UUID(value))
record=json.loads(get('/self/v1/requests/'+rid))
if record.get('request_id')!=rid or not record.get('archive_complete'): raise RuntimeError('old request identity or archive status changed')
retired=False;tenant=None
for side in ('request','response'):
 try: raw=get('/self/v1/requests/'+rid+'/archive/'+side)
 except urllib.error.HTTPError as error:
  reason=json.loads(error.read()).get('error',{}).get('reason')
  if error.code!=409 or reason!='raw_retired_semantic_archive': raise
  retired=True;tenant=tenant or dbtenant()
  prefix=f'tenants/{tenant}/semantic/v2/requests/{rid}/'
  fence=json.loads(s3(prefix+'retired-'+side+'.json'))
  if fence['tenant_id']!=tenant or fence['request_id']!=rid or fence['side']!=side: raise RuntimeError('retirement identity mismatch')
  coverage=json.loads(s3(prefix+str(fence['semantic_revision'])+'-coverage.json'))
  if coverage['raw_sources'][side]!=fence['source'] or coverage['publication']['request_id']!=rid or coverage['publication']['tenant_id']!=tenant or coverage['binding']['node_hash']!=fence['semantic_leaf'] or coverage['coverage']['projection_state']!='complete' or not coverage['coverage']['observed_terminal'] or coverage['coverage']['gaps']: raise RuntimeError('retirement coverage mismatch')
  source=fence['source'];locator=source['object_locator']
  if locator.endswith('.mtcz1'): raise RuntimeError('compressed source requires product decoder')
  if locator.startswith('stores/training/'): locator=locator.split('/',3)[3]
  raw=s3(locator)
  if len(raw)!=source['size_bytes']: raise RuntimeError('retirement source length mismatch')
 if hashlib.sha256(raw).hexdigest()!=data['sha256'][side]: raise RuntimeError('archive digest mismatch: '+side)
if retired:
 listing=json.loads(get('/self/v1/sessions?limit=100'));matched=[]
 for entry in listing['sessions']:
  if entry.get('unlinked') or not entry.get('cluster_id'): continue
  name=entry['session_id']; detail=json.loads(get('/self/v1/sessions/'+urllib.parse.quote(name,safe='')))
  if any(row.get('request_id')==rid for row in detail['requests']): matched.append(name)
 if len(matched)!=1: raise RuntimeError('canonical session identity missing or ambiguous')
 path='/self/v1/sessions/'+urllib.parse.quote(matched[0],safe='')+'/requests/'+rid+'/semantic-view'
 page=json.loads(get(path))
 if page.get('status')!='published': raise RuntimeError('semantic view not published')
 texts=[]
 while True:
  for item in page['instances']:
   parts=json.loads(get(path+'?'+urllib.parse.urlencode({'parts_cursor':item['atom_cursor']})))
   for part in parts.get('parts',[]):
    for field in part.get('fields',[]):
     preview=field.get('preview') or {}
     if preview.get('complete'): texts.append(preview.get('text',''))
     elif field.get('content_cursor'):
      raw=get(path+'?'+urllib.parse.urlencode({'content_cursor':field['content_cursor']}))
      frames=[json.loads(line) for line in raw.splitlines() if line]
      if not frames or not frames[-1].get('complete'): raise RuntimeError('semantic content incomplete')
      texts.append(''.join(frame.get('text','') for frame in frames))
  cursor=page.get('next_cursor')
  if not cursor: break
  page=json.loads(get(path+'?'+urllib.parse.urlencode({'cursor':cursor})))
 print(json.dumps({'semantic_text_count':len(texts),'semantic_text_lengths':[len(t) for t in texts],'request_marker_present':any(data['marker'] in t for t in texts),'response_text_length':len(data['response_text']),'response_present':bool(data['response_text']) and any(data['response_text'] in t for t in texts)}),flush=True)
 if not any(data['marker'] in text for text in texts) or not data['response_text'] or not any(data['response_text'] in text for text in texts): raise RuntimeError('semantic request/response content missing')
print(json.dumps({'request_id':rid,'original_request_and_archives':'matched','retired_semantic_verified':retired}))
