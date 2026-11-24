import {createHash} from 'node:crypto';
import {appendFileSync} from 'node:fs';
import {createStagingMock} from '../../load/staging-peak/mock.ts';
const {server}=createStagingMock();
server.prependListener('request',(req,res)=>{
 if (!req.url?.includes('/chat/completions')) return;
 const chunks:Buffer[]=[];req.on('data',(c:Buffer)=>chunks.push(Buffer.from(c)));
 req.on('end',()=>{
  const body=Buffer.concat(chunks);let marker='';try{marker=JSON.parse(body.toString()).messages[0].content}catch{}
  appendFileSync('/tmp/mtc-image-e2e-mock.jsonl',JSON.stringify({marker_sha256:createHash('sha256').update(marker).digest('hex'),bytes:body.length})+'\n');
 });
});
server.listen(18082,'127.0.0.1');
