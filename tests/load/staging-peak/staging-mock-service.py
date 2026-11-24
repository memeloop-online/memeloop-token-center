"""Create exactly one run-specific mock Service or delete it by UID precondition."""
import json, os, re, socket, subprocess, sys, time, urllib.request
meta=json.load(open(sys.argv[2],encoding='utf-8')); run=sys.argv[3]; action=sys.argv[1]
assert re.fullmatch(r'[a-z0-9-]{1,40}',run) and action in ('create','delete')
context=meta['context']; ns='memeloop-token-center-staging'; name='mtc-peak-'+run+'-mock'
def kubectl(*args,**kw):return subprocess.check_output(['kubectl','--context',context,*args],**kw)
def obj(*args):return json.loads(kubectl(*args,'-o','json'))
assert obj('get','namespace','kube-system')['metadata']['uid']==meta['clusterUid']
assert obj('get','namespace',ns)['metadata']['uid']==meta['namespaceUid']
if action=='create':
 service={'apiVersion':'v1','kind':'Service','metadata':{'name':name,'namespace':ns,'labels':{'app.kubernetes.io/part-of':'mtc-staging-peak','mtc.memeloop.ai/peak-run':run}},'spec':{'selector':{'app.kubernetes.io/part-of':'mtc-staging-peak','mtc.memeloop.ai/peak-run':run,'app.kubernetes.io/component':'mock'},'ports':[{'port':18080,'targetPort':8080}]}}
 created=json.loads(kubectl('-n',ns,'create','-f','-','-o','json',input=json.dumps(service).encode()))
 print(json.dumps({'name':name,'uid':created['metadata']['uid'],'namespaceUid':meta['namespaceUid'],'runId':run}))
else:
 uid=sys.argv[4];assert re.fullmatch(r'[a-f0-9-]{36}',uid)
 current=obj('-n',ns,'get','service',name);assert current['metadata']['uid']==uid and current['metadata']['labels']['mtc.memeloop.ai/peak-run']==run
 sock=socket.socket();sock.bind(('127.0.0.1',0));port=sock.getsockname()[1];sock.close()
 proxy=subprocess.Popen(['kubectl','--context',context,'proxy','--address=127.0.0.1','--port',str(port)],stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL)
 try:
  url=f'http://127.0.0.1:{port}/api/v1/namespaces/{ns}/services/{name}'
  for attempt in range(30):
   try:
    req=urllib.request.Request(url,data=json.dumps({'apiVersion':'v1','kind':'DeleteOptions','preconditions':{'uid':uid}}).encode(),headers={'Content-Type':'application/json'},method='DELETE')
    with urllib.request.urlopen(req,timeout=5) as response: assert response.status in (200,202)
    print(json.dumps({'name':name,'deletedUid':uid}));break
   except urllib.error.URLError:
    if attempt==29:raise
    time.sleep(.1)
 finally:proxy.terminate();proxy.wait(timeout=5)
