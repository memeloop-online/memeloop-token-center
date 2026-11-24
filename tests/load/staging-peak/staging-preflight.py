"""Fail-closed, read-only staging identity check; no request load or mutation."""
import ipaddress,json,subprocess,sys

def require(ok,message):
 if not ok: raise ValueError(message)
def verify_endpoint_slice(item,svc,namespace,pods):
 require(item['metadata'].get('labels',{}).get('endpointslice.kubernetes.io/managed-by')=='endpointslice-controller.k8s.io','untrusted EndpointSlice controller')
 require(any(o.get('kind')=='Service' and o.get('uid')==svc['metadata']['uid'] and o.get('controller') is True for o in item['metadata'].get('ownerReferences',[])),'foreign service owner')
 ports=item.get('ports',[]); require(len(ports)==1 and ports[0].get('port')==8080 and ports[0].get('protocol','TCP')=='TCP','unexpected endpoint port')
 family={'IPv4':4,'IPv6':6}.get(item.get('addressType'));require(family is not None,'unsupported address type')
 count=0
 for ep in item.get('endpoints',[]):
  ref=ep.get('targetRef',{});require(ref.get('kind')=='Pod' and ref.get('namespace',namespace)==namespace and ref.get('uid'),'endpoint is not a namespaced pod')
  pod=pods(ref['name']);require(pod['metadata']['uid']==ref['uid'],'pod uid mismatch')
  require(all(pod['metadata'].get('labels',{}).get(k)==v for k,v in svc['spec']['selector'].items()),'pod selector mismatch')
  actual={str(ipaddress.ip_address(x['ip'])) for x in pod.get('status',{}).get('podIPs',[])}
  if pod.get('status',{}).get('podIP'):actual.add(str(ipaddress.ip_address(pod['status']['podIP'])))
  addresses=ep.get('addresses',[]);require(bool(addresses),'empty endpoint addresses')
  for address in addresses:
   parsed=ipaddress.ip_address(address);require(parsed.version==family and str(parsed) in actual,'endpoint address does not belong to referenced pod')
  count+=1
 return count

def main():
 meta=json.load(open(sys.argv[1],encoding='utf-8'));ns='memeloop-token-center-staging'
 def get(*args):return json.loads(subprocess.check_output(['kubectl','--context',meta['context'],*args,'-o','json']))
 require(get('get','namespace','kube-system')['metadata']['uid']==meta['clusterUid'],'cluster UID mismatch')
 require(get('get','namespace',ns)['metadata']['uid']==meta['namespaceUid'],'namespace UID mismatch')
 for role in ('gateway','control'):
  name='mtc-staging-'+role;svc=get('-n',ns,'get','service',name)
  require(svc['spec'].get('type')!='ExternalName' and svc['spec'].get('selector')==meta[role+'Selector'] and bool(svc['spec'].get('selector')),'service selector mismatch')
  ports=svc['spec'].get('ports',[]);require(len(ports)==1 and ports[0].get('port')==8080 and ports[0].get('protocol','TCP')=='TCP','unexpected service port')
  slices=get('-n',ns,'get','endpointslices','-l','kubernetes.io/service-name='+name)['items'];require(bool(slices),'no EndpointSlices')
  count=sum(verify_endpoint_slice(item,svc,ns,lambda name:get('-n',ns,'get','pod',name)) for item in slices)
  require(count>0,'no endpoint pods');print(role,'verified_endpoints',count)
 print('staging identities verified; no load started')
if __name__=='__main__':main()
