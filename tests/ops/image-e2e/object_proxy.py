import http.server,http.client,threading,time
state={'mode':'normal','faults':0}
class Handler(http.server.BaseHTTPRequestHandler):
 def log_message(self,*args):pass
 def do_POST(self):self.handle_request()
 def do_PUT(self):self.handle_request()
 def do_GET(self):self.handle_request()
 def do_HEAD(self):self.handle_request()
 def do_DELETE(self):self.handle_request()
 def handle_request(self):
  if self.path.startswith('/__test__/'):
   mode=self.path.rsplit('/',1)[-1]
   if mode in ('normal','slow','fail'):
    state['mode']=mode;state['faults']=0;self.send_response(204);self.end_headers();return
   self.send_response(200);self.end_headers();self.wfile.write(str(state['faults']).encode());return
  if self.command=='PUT' and state['mode']!='normal':
   state['faults']+=1
   if state['mode']=='slow':time.sleep(3)
   else:self.send_response(503);self.end_headers();return
  length=int(self.headers.get('Content-Length','0')); data=self.rfile.read(length) if length else None
  c=http.client.HTTPConnection('127.0.0.1',19000,timeout=30);c.request(self.command,self.path,body=data,headers=dict(self.headers));r=c.getresponse();self.send_response(r.status)
  for k,v in r.getheaders():
   if k.lower() not in ('transfer-encoding','connection','server','date'):self.send_header(k,v)
  self.end_headers();self.wfile.write(r.read());c.close()
http.server.ThreadingHTTPServer(('127.0.0.1',19001),Handler).serve_forever()
