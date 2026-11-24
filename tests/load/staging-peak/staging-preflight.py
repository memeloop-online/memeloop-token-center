"""Fail-closed, read-only staging identity check; no request load or mutation."""
import json, subprocess, sys

meta=json.load(open(sys.argv[1],encoding='utf-8'))
ns='memeloop-token-center-staging'
def get(*args):
 return json.loads(subprocess.check_output(['kubectl','--context',meta['context'],*args,'-o','json']))
assert get('get','namespace','kube-system')['metadata']['uid']==meta['clusterUid']
assert get('get','namespace',ns)['metadata']['uid']==meta['namespaceUid']
for role in ('gateway','control'):
 name='mtc-staging-'+role
 svc=get('-n',ns,'get','service',name)
 assert svc['spec'].get('type')!='ExternalName' and svc['spec'].get('selector')==meta[role+'Selector'] and svc['spec']['selector']
 slices=get('-n',ns,'get','endpointslices','-l','kubernetes.io/service-name='+name)['items']
 assert slices
 count=0
 for item in slices:
  assert any(o.get('kind')=='Service' and o.get('uid')==svc['metadata']['uid'] for o in item['metadata'].get('ownerReferences',[]))
  for ep in item.get('endpoints',[]):
   ref=ep.get('targetRef',{}); assert ref.get('kind')=='Pod' and ref.get('namespace',ns)==ns and ref.get('uid')
   pod=get('-n',ns,'get','pod',ref['name']);assert pod['metadata']['uid']==ref['uid']
   assert all(pod['metadata'].get('labels',{}).get(k)==v for k,v in svc['spec']['selector'].items())
   count+=1
 assert count>0
 print(role,'verified_endpoints',count)
print('staging identities verified; no load started')
