import hashlib,json,os,urllib.request,urllib.error
with open(os.environ['E2E_IDENTITY_FILE']) as f: data=json.load(f)
base=os.environ.get('E2E_BASE','http://127.0.0.1:18080')
def get(path):
 req=urllib.request.Request(base+path,headers={'Authorization':'Bearer '+data['key']})
 with urllib.request.urlopen(req,timeout=40) as r:
  if r.status!=200: raise RuntimeError('unexpected HTTP status')
  return r.read()
rid=data['request_id']
record=json.loads(get('/self/v1/requests/'+rid))
if record.get('request_id')!=rid or not record.get('archive_complete'): raise RuntimeError('old request identity or archive status changed')
for side in ('request','response'):
 try:
  raw=get('/self/v1/requests/'+rid+'/archive/'+side)
 except urllib.error.HTTPError as error:
  try: reason=json.loads(error.read()).get('error',{}).get('reason','unknown')
  except (ValueError, UnicodeDecodeError): reason='unparseable'
  print(json.dumps({'request_id':rid,'archive_side':side,'http_status':error.code,'reason':reason}),flush=True)
  raise
 if hashlib.sha256(raw).hexdigest()!=data['sha256'][side]: raise RuntimeError('archive digest mismatch: '+side)
print(json.dumps({'request_id':rid,'original_request_and_archives':'matched'}))
