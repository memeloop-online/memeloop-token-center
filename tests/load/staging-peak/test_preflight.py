import importlib.util, pathlib, unittest
p=pathlib.Path(__file__).with_name('staging-preflight.py');spec=importlib.util.spec_from_file_location('preflight',p);m=importlib.util.module_from_spec(spec);spec.loader.exec_module(m)
class TestEndpoints(unittest.TestCase):
 def setUp(self):
  self.svc={'metadata':{'uid':'svc'},'spec':{'selector':{'app':'gateway'}}};self.pod={'metadata':{'uid':'pod','labels':{'app':'gateway'}},'status':{'podIP':'10.0.0.1','podIPs':[{'ip':'10.0.0.1'},{'ip':'fd00::1'}]}}
  self.slice={'addressType':'IPv4','metadata':{'labels':{'endpointslice.kubernetes.io/managed-by':'endpointslice-controller.k8s.io'},'ownerReferences':[{'kind':'Service','uid':'svc','controller':True}]},'ports':[{'port':8080,'protocol':'TCP'}],'endpoints':[{'addresses':['10.0.0.1'],'targetRef':{'kind':'Pod','namespace':'stage','name':'p','uid':'pod'}}]}
 def check(self):return m.verify_endpoint_slice(self.slice,self.svc,'stage',lambda name:self.pod)
 def test_accepts_pod_ip(self):self.assertEqual(self.check(),1)
 def test_rejects_foreign_ip(self):
  self.slice['endpoints'][0]['addresses']=['10.0.0.9']
  with self.assertRaises(ValueError):self.check()
 def test_rejects_address_family_mismatch(self):
  self.slice['endpoints'][0]['addresses']=['fd00::1']
  with self.assertRaises(ValueError):self.check()
 def test_rejects_manual_controller(self):
  self.slice['metadata']['labels']['endpointslice.kubernetes.io/managed-by']='manual'
  with self.assertRaises(ValueError):self.check()
 def test_rejects_wrong_port(self):
  self.slice['ports'][0]['port']=9999
  with self.assertRaises(ValueError):self.check()
if __name__=='__main__':unittest.main()
