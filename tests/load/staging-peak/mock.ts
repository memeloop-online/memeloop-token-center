import { createMockServer, MockState } from "../benchmark-memory.ts";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import type { IncomingMessage, ServerResponse } from "node:http";

/** Existing JSON/SSE/large-body mock, with isolated failure and slow-read routes. */
export function createStagingMock() {
  const state = new MockState();
  const server = createMockServer(state);
  const fallback = server.listeners("request")[0] as (request: IncomingMessage, response: ServerResponse) => void;
  server.removeAllListeners("request");
  const counters = { requests: 0, injected_errors: 0, injected_disconnects: 0 };
  server.on("request", (request, response) => {
    const execute = async () => {
      if (request.url === "/healthz") { response.writeHead(200); response.end("ok"); return; }
      if (request.url === "/identity") { response.setHeader("content-type", "application/json"); response.end(JSON.stringify({run_id:process.env.MTC_PEAK_RUN_ID??"contract",source_sha:process.env.MTC_HARNESS_SOURCE_SHA??null})); return; }
      if (request.url === "/evidence") { response.setHeader("content-type", "application/json"); response.end(JSON.stringify({ ...counters, active_streams: state.activeStreams, peak_streams: state.peakStreams })); return; }
      counters.requests++;
      const match = /^\/scenario\/(normal|slow-read|upstream-error|disconnect|tool-session)(\/.*)$/.exec(request.url ?? "");
      if (!match) { response.writeHead(404); response.end(); return; }
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
