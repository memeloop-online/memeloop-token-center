import { createMockServer, MockState, streamChatSse } from "../benchmark-memory.ts";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import type { IncomingMessage, ServerResponse } from "node:http";

/** Existing JSON/SSE/large-body mock, with isolated failure and slow-read routes. */
export function createStagingMock() {
  const state = new MockState();
  const server = createMockServer(state);
  const fallback = server.listeners("request")[0] as (request: IncomingMessage, response: ServerResponse) => void;
  server.removeAllListeners("request");
  const counters = { requests: 0, injected_errors: 0, injected_disconnects: 0, accepted_active:0, accepted_total:0, completed:0, emitted_bytes:0, emitted_chunks:0, backpressure_events:0, drain_wait_ms:0 };
  const streams:Record<string,any>[]=[];
  const timeline:Record<string,unknown>[]=[];
  const sample=()=>timeline.push({utc:new Date().toISOString(),accepted_active:counters.accepted_active,accepted_total:counters.accepted_total,completed:counters.completed,active_streams:state.activeStreams,emitted_bytes:counters.emitted_bytes,emitted_chunks:counters.emitted_chunks,backpressure_events:counters.backpressure_events,drain_wait_ms:counters.drain_wait_ms});
  const sampler=setInterval(sample,1000);sampler.unref();server.once("close",()=>clearInterval(sampler));
  server.on("request", (request, response) => {
    const execute = async () => {
      if (request.url === "/healthz") { response.writeHead(200); response.end("ok"); return; }
      if (request.url === "/identity") { response.setHeader("content-type", "application/json"); response.end(JSON.stringify({run_id:process.env.MTC_PEAK_RUN_ID??"contract",source_sha:process.env.MTC_HARNESS_SOURCE_SHA??null})); return; }
      if (request.url === "/evidence") { response.setHeader("content-type", "application/json"); response.end(JSON.stringify({ ...counters, timeline, streams, active_streams: state.activeStreams, peak_streams: state.peakStreams })); return; }
      counters.requests++;
      const match = /^\/scenario\/(normal|slow-read|upstream-error|disconnect|tool-session)(\/.*)$/.exec(request.url ?? "");
      if (!match) { response.writeHead(404); response.end(); return; }
      const progress:Record<string,any>={ordinal:counters.accepted_total,scenario:match[1],accepted_at:new Date().toISOString(),emitted_bytes:0,emitted_chunks:0};streams.push(progress);
      const originalWrite=response.write.bind(response);
      response.write=((...args:any[])=>{const chunk=args[0];counters.emitted_bytes+=Buffer.isBuffer(chunk)?chunk.length:Buffer.byteLength(String(chunk),typeof args[1]==="string"?args[1] as BufferEncoding:undefined);counters.emitted_chunks++;progress.emitted_bytes+=Buffer.isBuffer(chunk)?chunk.length:Buffer.byteLength(String(chunk));progress.emitted_chunks++;progress.first_write_at??=new Date().toISOString();progress.last_write_at=new Date().toISOString();const ok=(originalWrite as any)(...args);if(!ok){counters.backpressure_events++;const began=performance.now();let done=false;const finish=()=>{if(done)return;done=true;counters.drain_wait_ms+=performance.now()-began;response.off("drain",finish);response.off("close",finish);};response.once("drain",finish);response.once("close",finish);}return ok;}) as typeof response.write;
      counters.accepted_total++;counters.accepted_active++;
      let ended=false;const finish=()=>{if(ended)return;ended=true;progress.ended_at=new Date().toISOString();counters.accepted_active--;counters.completed++;};
      response.once("finish",finish);response.once("close",finish);
      request.url = match[2];
      if (match[1] === "upstream-error") { counters.injected_errors++; request.resume(); response.writeHead(503, { "content-type": "application/json", "retry-after": "1" }); response.end(JSON.stringify({ error: { type: "mock_unavailable", message: "isolated test upstream" } })); return; }
      if (match[1] === "disconnect") { counters.injected_disconnects++; request.destroy(); return; }
      if (match[1] === "tool-session") {
        const chunks: Buffer[]=[]; for await(const chunk of request) chunks.push(Buffer.from(chunk));
        const body=JSON.parse(Buffer.concat(chunks).toString("utf8"));
        const followup=body.messages?.at(-1)?.role==="tool";
        if(followup && body.messages.at(-1).tool_call_id!=="call_staging_echo") { response.writeHead(400); response.end(); return; }
        const message=followup?{role:"assistant",content:"tool-complete"}:{role:"assistant",content:null,tool_calls:[{id:"call_staging_echo",type:"function",function:{name:"staging_echo",arguments:'{"value":"synthetic"}'}}]};
        response.writeHead(200,{"content-type":"application/json"});
        response.end(JSON.stringify({id:"chatcmpl-staging-tool",object:"chat.completion",model:body.model,choices:[{index:0,message,finish_reason:followup?"stop":"tool_calls"}],usage:{prompt_tokens:2,completion_tokens:1,total_tokens:3}})); return;
      }
      if (match[1] === "slow-read") { request.pause(); await delay(250); request.resume(); }
      if(request.url==="/v1/chat/completions") {
        const chunks:Buffer[]=[];for await(const chunk of request)chunks.push(Buffer.from(chunk));const body=JSON.parse(Buffer.concat(chunks).toString("utf8"));
        if(body.stream||body.benchmark?.mode==="stream"){await streamChatSse(request,response,state,Number(body.benchmark?.bytes),Math.max(4096,Math.min(Number(body.benchmark?.chunk_bytes??4096),1048576)),Number(body.benchmark?.delay_ms??0));return;}
        const text=body.benchmark?"x".repeat(Number(body.benchmark.bytes)):"ok";
        response.writeHead(200,{"content-type":"application/json"});response.end(JSON.stringify({id:"chatcmpl-staging",object:"chat.completion",model:body.model,choices:[{index:0,message:{role:"assistant",content:text},finish_reason:"stop"}],usage:{prompt_tokens:2,completion_tokens:1,total_tokens:3}}));return;
      }
      if(request.url==="/v1/responses") {
        const chunks:Buffer[]=[]; for await(const chunk of request) chunks.push(Buffer.from(chunk));
        const body=JSON.parse(Buffer.concat(chunks).toString("utf8"));
        const text="x".repeat(Number(body.benchmark?.bytes??2));
        const item={id:"msg_staging",type:"message",status:"completed",role:"assistant",content:[{type:"output_text",text,annotations:[]}]};
        const complete={id:"resp_staging",object:"response",status:"completed",model:body.model,output:[item],usage:{input_tokens:2,output_tokens:1,total_tokens:3}};
        if(!body.stream){response.writeHead(200,{"content-type":"application/json"});response.end(JSON.stringify(complete));return;}
        response.writeHead(200,{"content-type":"text/event-stream"}); let sequence=0;
        const event=async(type:string,data:Record<string,unknown>)=>{
          if(response.destroyed)return;
          const written=response.write(`event: ${type}\ndata: ${JSON.stringify({type,sequence_number:sequence++,...data})}\n\n`);
          if(!written)await new Promise<void>(resolve=>{const done=()=>{response.off("drain",done);response.off("close",done);resolve();};response.once("drain",done);response.once("close",done);});
        };
        state.begin(); try{
          await event("response.created",{response:{...complete,status:"in_progress",output:[]}});
          await event("response.output_item.added",{output_index:0,item:{...item,status:"in_progress",content:[]}});
          await event("response.content_part.added",{item_id:item.id,output_index:0,content_index:0,part:{type:"output_text",text:"",annotations:[]}});
          for(let offset=0;offset<text.length&&!response.destroyed;offset+=4096){await event("response.output_text.delta",{item_id:item.id,output_index:0,content_index:0,delta:text.slice(offset,offset+4096)});if(body.benchmark?.delay_ms)await delay(Number(body.benchmark.delay_ms));}
          if(response.destroyed)return;
          await event("response.output_text.done",{item_id:item.id,output_index:0,content_index:0,text});
          await event("response.content_part.done",{item_id:item.id,output_index:0,content_index:0,part:item.content[0]});
          await event("response.output_item.done",{output_index:0,item});
          await event("response.completed",{response:complete});response.end();
        }finally{state.end();}return;
      }
      fallback(request, response);
    };
    void execute().catch(() => { if (!response.headersSent) response.writeHead(500); response.end(); });
  });
  return { server, state, counters };
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const port = Number(process.env.MTC_MOCK_PORT ?? "8080");
  const { server } = createStagingMock();
  server.listen(port, "0.0.0.0");
  for (const signal of ["SIGINT", "SIGTERM"] as const) process.once(signal, () => { server.closeAllConnections(); server.close(() => process.exit(0)); });
}
