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
  const plan=validatePlan({release_batch:"synthetic-contract-only",window_ms:1000,drain_timeout_ms:1000,baseline:null,arrivals:[{protocol:"chat",at_ms:0,scenario:"normal",session:"unit",stream:false,request_padding_bytes:0,response_fixture_bytes:0,chunk_delay_ms:0,client_read_delay_ms:0,expected_status:200}]});
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

import { artifactHash } from "./identity.ts";
import { FixtureVerifier } from "./fixture.ts";
test("qualifier rejects observations from another image, plan or baseline",()=>{
  const candidate=Object.fromEntries(["gateway","control","worker"].map(role=>[role,{source_sha:"b".repeat(40),image_digest:"sha256:"+"c".repeat(64)}]));
  const plan={candidate,candidate_evidence:{candidate,observed_at:"2026-01-01T00:00:00Z"},baseline:null};
  const load={run_id:"bound",source_sha:"a".repeat(40),harness_source_sha:"a".repeat(40),candidate,plan_sha256:artifactHash(plan),baseline_sha256:artifactHash(null),candidate_evidence_sha256:artifactHash(plan.candidate_evidence),offered:0,started:0,unissued:0,failed:0};
  const observer={run_id:load.run_id,source_sha:load.source_sha,harness_source_sha:load.harness_source_sha,candidate_before:candidate,candidate_after:candidate,plan_sha256:load.plan_sha256,baseline_sha256:load.baseline_sha256,candidate_evidence_sha256:load.candidate_evidence_sha256};
  assert.equal(qualify(load,observer,null,plan).qualification,"NOT_EVALUATED");
  for(const patch of [{plan_sha256:"d".repeat(64)},{baseline_sha256:"e".repeat(64)},{candidate_after:{...candidate,gateway:{source_sha:"f".repeat(40),image_digest:"sha256:"+"1".repeat(64)}}}])assert.equal(qualify(load,{...observer,...patch},null,plan).qualification,"FAILED");
});

test("buffered fixture sends actual large JSON and verifies payload size/content",async()=>{
  const {server}=createStagingMock();await new Promise<void>(resolve=>server.listen(0,"127.0.0.1",resolve));const address=server.address();assert(address&&typeof address!=="string");
  try{for(const protocol of ["chat","responses"] as const){const bytes=256*1024;const response=await fetch(`http://127.0.0.1:${address.port}/scenario/normal/v1/${protocol==="chat"?"chat/completions":"responses"}`,{method:"POST",body:JSON.stringify({model:"synthetic",stream:false,benchmark:{mode:"buffered",bytes}})});const wire=new Uint8Array(await response.arrayBuffer());assert(wire.byteLength>bytes);const verifier=new FixtureVerifier(protocol,false,bytes);verifier.consume(wire);const result=verifier.finish(wire.byteLength);assert.equal(result.verified,true);assert.equal(result.response_payload_bytes,bytes);const wrong=new FixtureVerifier(protocol,false,bytes+1);wrong.consume(wire);assert.equal(wrong.finish(wire.byteLength).verified,false);}}finally{server.closeAllConnections();await new Promise<void>(resolve=>server.close(()=>resolve()));}
});

import { targetEndpoints } from "./run.ts";
test("isolated endpoints bind namespace to exact services", () => {
  const env = { POD_NAMESPACE: "memeloop-token-center-staging", MTC_PEAK_RUN_ID: "tool-fixture-20261118" };
  const endpoints = targetEndpoints(env);
  assert.equal(endpoints.forwarded, false);
  assert.equal(endpoints.gateway.href, "http://mtc-staging-gateway:8080/");
  assert.equal(endpoints.control.href, "http://mtc-staging-control:8080/");
  assert.equal(endpoints.mock.href, "http://mtc-staging-mock:18080/");
  assert.equal(targetEndpoints({ ...env, POD_NAMESPACE: "memeloop-token-center-test" }).gateway.hostname, "memeloop-token-center-test-gateway");
  for (const invalid of [
    { ...env, POD_NAMESPACE: "memeloop-token-center" },
    { ...env, POD_NAMESPACE: undefined },
    { ...env, MTC_PEAK_RUN_ID: undefined },
    { ...env, MTC_TEST_GATEWAY_URL: "http://memeloop-token-center-test-gateway:8080" },
    { ...env, MTC_TEST_CONTROL_URL: "http://mtc-staging-control:8080/private" },
    { ...env, MTC_TEST_MOCK_URL: "http://mtc-staging-mock:8080" },
    { ...env, MTC_PEAK_OPERATOR_FORWARD: "1" },
  ]) assert.throws(() => targetEndpoints(invalid));
});

test("operator forwarding remains explicit in report inputs", () => {
  const endpoints = targetEndpoints({ POD_NAMESPACE: "memeloop-token-center-test", MTC_PEAK_RUN_ID: "unit", MTC_PEAK_OPERATOR_FORWARD: "1", MTC_TEST_GATEWAY_URL: "http://127.0.0.1:18090", MTC_TEST_CONTROL_URL: "http://127.0.0.1:18089", MTC_TEST_MOCK_URL: "http://127.0.0.1:18091", MTC_PEAK_UPSTREAM_BASE_URL: "http://10.42.8.1:18080", MTC_PEAK_TEST_GATEWAY_POD_IP: "10.42.8.1" });
  assert.equal(endpoints.forwarded, true);
});
