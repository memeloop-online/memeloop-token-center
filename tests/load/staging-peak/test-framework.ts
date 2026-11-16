import { test } from "node:test";
import assert from "node:assert/strict";
import { validateBaseline, baselineIdentity } from "./baseline.ts";
import { createStagingMock } from "./mock.ts";

test("missing natural peak evidence cannot become a passing baseline", () => {
  for (const value of [null, {}, { schema_version: 1, windows: [] }]) assert.throws(() => validateBaseline(value));
  const measured = { schema_version: 1 as const, evidence: { source: "synthetic-unit-contract-only", observed_from: "2026-01-01T00:00:00Z", observed_until: "2026-01-01T01:00:00Z", query_sha256: "a".repeat(64) }, windows: [{ seconds: 60, requests: 120, max_in_flight: 4, input_bytes: 1000, output_bytes: 2000 }], protocol_requests: { chat: 120 } };
  assert.equal(baselineIdentity(measured).length, 64);
  assert.throws(() => validateBaseline({ ...measured, windows: [{ ...measured.windows[0], requests: 0 }] }));
});

test("external staging mock reuses gateway JSON/SSE contracts and isolates failures", async () => {
  const { server } = createStagingMock();
  await new Promise<void>((resolve) => server.listen(0, "127.0.0.1", resolve));
  const address = server.address(); assert(address && typeof address !== "string");
  const url = `http://127.0.0.1:${address.port}`;
  try {
    const json = await fetch(`${url}/scenario/normal/v1/chat/completions`, { method: "POST", body: JSON.stringify({ model: "isolated-mock", messages: [{ role: "user", content: "test" }] }) });
    assert.equal(json.status, 200); assert.equal((await json.json() as any).choices[0].message.content, "ok");
    const sse = await fetch(`${url}/scenario/normal/v1/chat/completions`, { method: "POST", body: JSON.stringify({ model: "isolated-mock", benchmark: { mode: "stream", bytes: 8192, chunk_bytes: 4096 } }) });
    assert.equal(sse.status, 200); assert.match(await sse.text(), /data: \[DONE\]/);
    const failure = await fetch(`${url}/scenario/upstream-error/v1/chat/completions`, { method: "POST", body: "{}" });
    assert.equal(failure.status, 503); await failure.arrayBuffer();
    const unknown = await fetch(`${url}/v1/chat/completions`, { method: "POST", body: "{}" });
    assert.equal(unknown.status, 404);
  } finally { server.closeAllConnections(); await new Promise<void>((resolve) => server.close(() => resolve())); }
});

import { validatePlan } from "./run.ts";
import { manifests } from "./manifests.ts";
test("TEST-only immutable manifests never mount production secrets or service-account tokens", () => {
  const plan=validatePlan({release_batch:"synthetic-contract-only",window_ms:1000,drain_timeout_ms:1000,baseline:null,arrivals:[{protocol:"chat",at_ms:0,scenario:"normal",session:"unit",stream:false,request_bytes:0,response_bytes:0,chunk_delay_ms:0,client_read_delay_ms:0,expected_status:200}]});
  const bundle=manifests("contract","example.invalid/test@sha256:"+"a".repeat(64),"b".repeat(40),plan);
  for(const item of bundle.items) assert.equal(item.metadata.namespace,"memeloop-token-center-test");
  const serialized=JSON.stringify(bundle); assert(!serialized.includes("memeloop-token-center-secrets"));
  for(const item of bundle.items.filter(item=>["Job","Deployment"].includes(item.kind))) assert.equal((item as any).spec.template.spec.automountServiceAccountToken,false);
  assert.throws(()=>manifests("contract","example.invalid/test:latest","b".repeat(40),plan));
  assert.throws(()=>validatePlan({...plan,arrivals:[{...plan.arrivals[0],at_ms:1001}]}));
});

test("tool session mock preserves the tool-call identity through its result", async()=>{
  const {server}=createStagingMock(); await new Promise<void>(resolve=>server.listen(0,"127.0.0.1",resolve));
  const address=server.address(); assert(address&&typeof address!=="string"); const url=`http://127.0.0.1:${address.port}/scenario/tool-session/v1/chat/completions`;
  try {
    const first=await fetch(url,{method:"POST",body:JSON.stringify({model:"synthetic",messages:[{role:"user",content:"tool"}]})});
    const message=(await first.json() as any).choices[0].message; assert.equal(message.tool_calls[0].id,"call_staging_echo");
    const second=await fetch(url,{method:"POST",body:JSON.stringify({model:"synthetic",messages:[message,{role:"tool",tool_call_id:message.tool_calls[0].id,content:"synthetic"}]})});
    assert.equal((await second.json() as any).choices[0].message.content,"tool-complete");
  }finally{server.closeAllConnections();await new Promise<void>(resolve=>server.close(()=>resolve()));}
});


test("native Responses SSE exposes completed usage and output",async()=>{
  const {server}=createStagingMock();await new Promise<void>(resolve=>server.listen(0,"127.0.0.1",resolve));
  const address=server.address();assert(address&&typeof address!=="string");
  try{const response=await fetch(`http://127.0.0.1:${address.port}/scenario/normal/v1/responses`,{method:"POST",body:JSON.stringify({model:"synthetic",input:"test",stream:true,benchmark:{bytes:8192}})});const text=await response.text();assert.match(text,/event: response.completed/);assert.match(text,/"input_tokens":2/);assert.match(text,/"output_text"/);}finally{server.closeAllConnections();await new Promise<void>(resolve=>server.close(()=>resolve()));}
});

import { referencePlan } from "./reference-plan.ts";
test("sampled reference phases preserve lower-bound qualification gaps",()=>{
  const plan=referencePlan("synthetic-unit-only",1.7,91);
  assert.equal(plan.arrivals.filter(row=>row.at_ms===0).length,91);
  assert.equal(plan.arrivals.filter(row=>row.at_ms>=30_000&&row.at_ms<90_000).length,102);
  assert.equal(plan.baseline,null);
  assert.throws(()=>referencePlan("",1.7,91));
});

import { qualify } from "./report.ts";
test("missing observer or real baseline cannot qualify the release",()=>{
  const report=qualify({run_id:"contract",source_sha:"a".repeat(40),offered:1,started:1,unissued:0,failed:0,requests:[]},null,null);
  assert.equal(report.qualification,"NOT_EVALUATED");assert(report.missing_evidence.includes("complete_historical_baseline"));
  assert.equal(qualify({run_id:"contract",offered:1,started:1,unissued:0,failed:1},null,null).qualification,"FAILED");
});
