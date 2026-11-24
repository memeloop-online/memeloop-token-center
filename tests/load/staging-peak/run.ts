import { reuseFixture } from "./fixture-setup.ts";
import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { artifactHash, candidateIdentity, type Candidate } from "./identity.ts";
import { FixtureVerifier } from "./fixture.ts";
import { validateBaseline, baselineIdentity } from "./baseline.ts";

type Phase = "plan_read" | "plan_validate" | "environment" | "mock_identity" | "candidate_identity" | "control_version" | "upstream_setup" | "route_setup" | "price_setup" | "key_setup" | "load" | "report_write";
let phase: Phase = "plan_read";
let httpStatus: number | undefined;
let setupIndex: number | undefined;
let setupScenario: Arrival["scenario"] | undefined;
function enter(next: Phase) { phase = next; httpStatus = undefined; }
export function failureDiagnostic(error: unknown) {
  const names = ["Error", "TypeError", "SyntaxError", "AbortError", "TimeoutError"];
  const networks = ["ECONNREFUSED", "ECONNRESET", "ETIMEDOUT", "ENOTFOUND", "EAI_AGAIN", "UND_ERR_CONNECT_TIMEOUT", "UND_ERR_HEADERS_TIMEOUT", "UND_ERR_SOCKET"];
  const e = error as { name?: unknown; cause?: { code?: unknown } } | null;
  const errorName = typeof e?.name === "string" && names.includes(e.name) ? e.name : undefined;
  const networkCode = typeof e?.cause?.code === "string" && networks.includes(e.cause.code) ? e.cause.code : undefined;
  return { event: "staging_peak_failed", phase, code: httpStatus && httpStatus >= 400 ? "http_failure" : networkCode ? "network_failure" : errorName === "SyntaxError" ? "invalid_json" : "phase_failure", http_status: httpStatus, error_name: errorName, network_code: networkCode, scenario: setupScenario, setup_index: setupIndex };
}

export type Arrival = { protocol: "chat" | "responses"; at_ms: number; scenario: "normal" | "slow-read" | "upstream-error" | "disconnect" | "tool-session"; session: string; stream: boolean; request_padding_bytes: number; response_fixture_bytes: number; chunk_delay_ms: number; client_read_delay_ms: number; expected_status: number };
export type Plan = { candidate?:Candidate; candidate_evidence?:{candidate:Candidate;observed_at:string}; release_batch: string; window_ms: number; drain_timeout_ms: number; arrivals: Arrival[]; baseline: unknown };
export function validatePlan(value: unknown): Plan {
  const plan = value as Plan;
  if (!plan?.release_batch || !Number.isSafeInteger(plan.window_ms) || plan.window_ms <= 0 || !Number.isSafeInteger(plan.drain_timeout_ms) || plan.drain_timeout_ms <= 0 || !Array.isArray(plan.arrivals) || !plan.arrivals.length) throw new Error("explicit release batch, arrival window and drain period required");
  let previous = -1;
  for (const row of plan.arrivals) {
    if (!Number.isSafeInteger(row.at_ms) || row.at_ms < previous || row.at_ms >= plan.window_ms || !["normal", "slow-read", "upstream-error", "disconnect", "tool-session"].includes(row.scenario) || !row.session || typeof row.stream !== "boolean" || !["chat","responses"].includes(row.protocol)) throw new Error("invalid ordered arrival or scenario");
    for (const number of [row.request_padding_bytes, row.response_fixture_bytes, row.chunk_delay_ms, row.client_read_delay_ms]) if (!Number.isSafeInteger(number) || number < 0) throw new Error("invalid measured workload size or delay");
    if (!Number.isInteger(row.expected_status) || row.expected_status < 100 || row.expected_status > 599) throw new Error("expected gateway result required");
    if(row.scenario==="tool-session" && (row.stream || row.protocol!=="chat")) throw new Error("tool conversation fixture requires buffered replies");
    if (row.stream && row.response_fixture_bytes < 4096) throw new Error("SSE workload must include protocol framing");
    previous = row.at_ms;
  }
  return plan;
}
const percentile = (values: number[], p: number) => values.length ? [...values].sort((a,b) => a-b)[Math.ceil(values.length*p)-1] : null;

export function targetEndpoints(env: NodeJS.ProcessEnv) {
  const staging = env.POD_NAMESPACE === "memeloop-token-center-staging";
  if (!staging && env.POD_NAMESPACE !== "memeloop-token-center-test") throw new Error("isolated workload namespace required");
  const runId = env.MTC_PEAK_RUN_ID;
  if (!runId || !/^[a-z0-9-]{1,40}$/.test(runId)) throw new Error("DNS-safe explicit run identity required");
  const gateway = new URL(env.MTC_TEST_GATEWAY_URL ?? (staging ? "http://mtc-staging-gateway:8080" : "http://memeloop-token-center-test-gateway:8080"));
  const control = new URL(env.MTC_TEST_CONTROL_URL ?? (staging ? "http://mtc-staging-control:8080" : "http://memeloop-token-center-test-control:8080"));
  const mock = new URL(env.MTC_TEST_MOCK_URL ?? (staging ? `http://mtc-peak-${runId}-mock:18080` : `http://mtc-peak-${runId}-mock:8080`));
  const forwarded = env.MTC_PEAK_OPERATOR_FORWARD === "1";
  const upstreamBase = forwarded ? new URL(env.MTC_PEAK_UPSTREAM_BASE_URL ?? "") : mock;
  if (forwarded && staging) throw new Error("staging requires direct isolated services");
  if (forwarded) {
    for (const [url, port] of [[gateway,"18090"],[control,"18089"],[mock,"18091"]] as const) if (url.protocol !== "http:" || url.hostname !== "127.0.0.1" || url.port !== port || url.pathname !== "/" || url.username || url.password || url.search || url.hash) throw new Error("invalid operator forward endpoint");
    if (upstreamBase.protocol !== "http:" || upstreamBase.hostname !== env.MTC_PEAK_TEST_GATEWAY_POD_IP || !/^10\.42\.\d{1,3}\.\d{1,3}$/.test(upstreamBase.hostname) || upstreamBase.port !== "18080" || upstreamBase.pathname !== "/" || upstreamBase.username || upstreamBase.password || upstreamBase.search || upstreamBase.hash) throw new Error("invalid TEST sidecar upstream");
  } else {
    for (const [url, host] of [[gateway,staging ? "mtc-staging-gateway" : "memeloop-token-center-test-gateway"],[control,staging ? "mtc-staging-control" : "memeloop-token-center-test-control"],[mock,`mtc-peak-${runId}-mock`]] as const) if (url.protocol !== "http:" || url.hostname !== host || url.port !== (staging && url === mock ? "18080" : "8080") || url.pathname !== "/" || url.username || url.password || url.search || url.hash) throw new Error("isolated service identity mismatch");
  }
  return { runId, gateway, control, mock, upstreamBase, forwarded };
}

export async function run(plan: Plan, env = process.env): Promise<Record<string, unknown>> {
  enter("plan_validate");
  validatePlan(plan);
  enter("environment");
  const { runId, gateway, control, mock, upstreamBase, forwarded } = targetEndpoints(env);
  enter("mock_identity");
  const identityResponse=await fetch(new URL("/identity",mock),{signal:AbortSignal.timeout(10_000)});
  httpStatus=identityResponse.status;
  const identity=await identityResponse.json() as {run_id?:string;source_sha?:string};
  if(!identityResponse.ok || identity.run_id!==runId || identity.source_sha!==env.MTC_HARNESS_SOURCE_SHA) throw new Error("exact-run mock identity mismatch");
  const token = env.MTC_TEST_SERVICE_TOKEN;
  if (!token) throw new Error("TEST service token secret reference required");
  const call = async (stage: Phase, path: string, payload: unknown) => {
    enter(stage);
    const response = await fetch(new URL(path, control), { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type":"application/json" }, body: JSON.stringify(payload), signal: AbortSignal.timeout(30_000) });
    httpStatus=response.status;
    if (!response.ok) throw new Error(`isolated control setup HTTP ${response.status}`);
    return await response.json() as Record<string, any>;
  };
  enter("candidate_identity");
  const expectedCandidate=candidateIdentity(plan.candidate);
  const actualCandidate=candidateIdentity(plan.candidate_evidence?.candidate);
  if(!Number.isFinite(Date.parse(plan.candidate_evidence?.observed_at??""))||artifactHash(expectedCandidate)!==artifactHash(actualCandidate))throw new Error("candidate observation does not match this workload plan");
  enter("control_version");
  const versionResponse=await fetch(new URL("/version",control),{headers:{authorization:`Bearer ${token}`},signal:AbortSignal.timeout(10_000)});
  httpStatus=versionResponse.status;
  const version=await versionResponse.json() as {revision?:string};
  if(!versionResponse.ok||version.revision!==actualCandidate.control.source_sha)throw new Error("actual control revision differs from observed candidate");
  const fixtureId=env.MTC_PEAK_FIXTURE_ID??runId;
  if(!/^[a-zA-Z0-9:_-]+$/.test(fixtureId))throw new Error("invalid isolated fixture identity");
  const tenant = `staging-peak-${fixtureId}`;
  enter("upstream_setup");
  const reused = await reuseFixture(control, token, tenant, fixtureId, upstreamBase, [...new Set(plan.arrivals.map(row => row.scenario))], plan.arrivals.length);
  const models: Record<string,string> = {}, routes: string[] = [];
  for (const scenario of new Set(plan.arrivals.map(row => row.scenario))) {
    setupScenario=scenario; setupIndex=(setupIndex ?? -1)+1;
    const model = `staging-peak-${fixtureId}-${scenario}`;
    if (reused) { models[scenario]=model; routes.push(reused.routes[scenario]); continue; }
    const upstream = await call("upstream_setup", "/internal/v1/upstreams", { tenant_external_id:tenant, name:model, driver:"http-json", config:{ base_url:new URL(`/scenario/${scenario}`,upstreamBase).href, network_scope:"private" }, credential:{ type:"none" } });
    const route = await call("route_setup", "/internal/v1/model-routes", { tenant_external_id:tenant, public_model:model, upstream_account_id:upstream.id, upstream_model:model, protocol:"openai", priority:0, custom_model_confirmed:true });
    await call("price_setup", `/internal/v1/prices/USD/${model}`, { input_per_million:"0.01", output_per_million:"0.02" });
    models[scenario]=model; routes.push(String(route.id));
  }
  setupScenario=undefined; setupIndex=undefined;
  const issued = reused ? {key:reused.key} : await call("key_setup", "/internal/v1/keys", { tenant_external_id:tenant, principal_external_id:`staging-peak-${fixtureId}`, alias:`Isolated ${fixtureId}`, currency:"USD", initial_balance:"100000", route_ids:routes, policy:{requests_per_minute:Math.max(1,plan.arrivals.length*60), tokens_per_minute:1_000_000_000, max_concurrency:plan.arrivals.length, enforcement_mode:"metered_unlimited", daily_budget:null, weekly_budget:null, lifetime_budget:null} });
  enter("load");
  const key = String(issued.key);
  const requests: Record<string,unknown>[] = [];
  let active=0, peakActive=0;
  const start=performance.now();
  const timeline:Record<string,unknown>[]=[];
  const sample=()=>{const counts:Record<string,number>={};for(const r of requests)counts[String(r.status??"transport")]=(counts[String(r.status??"transport")]??0)+1;timeline.push({utc:new Date().toISOString(),elapsed_ms:performance.now()-start,offered:plan.arrivals.filter(a=>a.at_ms<=performance.now()-start).length,client_active:active,started:requests.length+active,completed:requests.length,status_counts:counts});};
  const sampler=setInterval(sample,1000);sample();
  const tasks: Promise<void>[]=[];
  for (const [index, arrival] of plan.arrivals.entries()) {
    const remaining=start+arrival.at_ms-performance.now(); if (remaining>0) await delay(remaining);
    const lag=performance.now()-start-arrival.at_ms;
    active++; peakActive=Math.max(peakActive,active);
    tasks.push((async () => {
      const began=performance.now(), session=`${runId}:${arrival.session}`;
      let status=0, bytes=0, exchanges=1, requestId:string|null=null, error:string|undefined, hash=createHash("sha256"), tail="", terminal=false, serializedRequestBytes=0, fixtureEvidence:Record<string,unknown>={};
      const verifier=new FixtureVerifier(arrival.protocol,arrival.stream,arrival.response_fixture_bytes);
      try {
        const payload: Record<string,unknown>={ model:models[arrival.scenario], messages:[{role:"user",content:"synthetic peak replay"+"x".repeat(arrival.request_padding_bytes)}], metadata:{session_id:session}, stream:arrival.stream, max_tokens:1 };
        payload.benchmark={mode:arrival.stream?"stream":"buffered",bytes:arrival.response_fixture_bytes,chunk_bytes:4096,delay_ms:arrival.chunk_delay_ms};
        if(arrival.protocol==="responses"){payload.input=payload.messages;delete payload.messages;}
        const serialized=JSON.stringify(payload);serializedRequestBytes=Buffer.byteLength(serialized);
        const response=await fetch(new URL(arrival.protocol==="responses"?"/v1/responses":"/v1/chat/completions",gateway), {method:"POST",headers:{authorization:`Bearer ${key}`,"content-type":"application/json","x-mtc-conversation-id":session},body:serialized,signal:AbortSignal.timeout(plan.window_ms+plan.drain_timeout_ms)});
        status=response.status;requestId=response.headers.get("x-mtc-request-id");
        if (response.body) for await (const chunk of response.body) { bytes+=chunk.length; hash.update(chunk); if(status===200&&arrival.scenario!=="tool-session")verifier.consume(chunk); const scan=tail+Buffer.from(chunk).toString("utf8");terminal ||= arrival.protocol==="responses"?scan.includes('"type":"response.completed"'):scan.includes("data: [DONE]");tail=scan.slice(-4096); if(arrival.client_read_delay_ms) await delay(arrival.client_read_delay_ms); }
        if(status===200&&arrival.scenario!=="tool-session"){fixtureEvidence=verifier.finish(bytes);if(fixtureEvidence.verified!==true)error="fixture_content_or_size_mismatch";}
        if(status!==arrival.expected_status) error="unexpected_gateway_status";
        if(arrival.stream && status===200 && !terminal) error="missing_stream_terminal";
        if(arrival.scenario==="tool-session" && status===200) {
          const message=JSON.parse(tail).choices[0].message;
          const tool=message.tool_calls?.[0];
          if(tool?.id!=="call_staging_echo" || tool.function?.name!=="staging_echo") throw new Error("tool_identity_mismatch");
          exchanges++;
          const followup=await fetch(new URL("/v1/chat/completions",gateway),{method:"POST",headers:{authorization:`Bearer ${key}`,"content-type":"application/json","x-mtc-conversation-id":session},body:JSON.stringify({model:models[arrival.scenario],metadata:{session_id:session},messages:[{role:"user",content:"synthetic tool session"},message,{role:"tool",tool_call_id:tool.id,content:'{"value":"synthetic"}'}]}),signal:AbortSignal.timeout(plan.drain_timeout_ms)});
          const completed=await followup.json() as any;
          if(followup.status!==200 || completed.choices?.[0]?.message?.content!=="tool-complete") error="tool_followup_mismatch";
        }
      } catch { error="transport_or_timeout"; }
      finally { active--; requests.push({index,session,...fixtureEvidence,request_padding_bytes:arrival.request_padding_bytes,serialized_request_bytes:serializedRequestBytes,protocol:arrival.protocol,http_exchanges:exchanges,request_id:requestId,scenario:arrival.scenario,offered_at_ms:arrival.at_ms,started_at_ms:began-start,generator_lag_ms:lag,status,response_wire_bytes:bytes,response_sha256:hash.digest("hex"),latency_ms:performance.now()-began,error}); }
    })());
  }
  try { await Promise.all(tasks); } finally { clearInterval(sampler); sample(); }
  if(performance.now()-start<plan.window_ms) await delay(plan.window_ms-(performance.now()-start));
  let baselineHash:string|undefined, baselineError:string|undefined;
  try { baselineHash=baselineIdentity(validateBaseline(plan.baseline)); } catch { baselineError="historical_peak_coverage_incomplete"; }
  const latencies=requests.map(row=>Number(row.latency_ms));
  return {topology:forwarded?"same-pod-mock+port-forward":"cross-pod",schema_version:1,run_id:runId,release_batch:plan.release_batch,source_sha:env.MTC_HARNESS_SOURCE_SHA??null,harness_source_sha:env.MTC_HARNESS_SOURCE_SHA??null,candidate:actualCandidate,candidate_evidence_sha256:artifactHash(plan.candidate_evidence),plan_sha256:artifactHash(plan),tenant,fixture_id:fixtureId,timeline,offered:plan.arrivals.length,started:requests.length,completed:requests.filter(row=>!row.error).length,failed:requests.filter(row=>row.error).length,unissued:plan.arrivals.length-requests.length,peak_active:peakActive,http_exchanges:requests.reduce((sum,row)=>sum+Number(row.http_exchanges),0),latency_ms:{p50:percentile(latencies,.5),p95:percentile(latencies,.95),p99:percentile(latencies,.99),max:Math.max(...latencies)},baseline_sha256:artifactHash(plan.baseline),validated_baseline_sha256:baselineHash,baseline_error:baselineError,qualification:"NOT_EVALUATED",missing_evidence:["complete historical multidimensional peak","financial exactly-once assertions","semantic/archive API payload verification","PG poolwait/IO/locks and memory recovery","archive debt drain confirmation"],requests};
}
if(process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href) {
  try { enter("plan_read"); const plan=JSON.parse(readFileSync(process.env.MTC_PEAK_PLAN_FILE??"/plan/plan.json","utf8")); enter("plan_validate"); const report=await run(validatePlan(plan)); enter("report_write"); writeFileSync(process.env.MTC_PEAK_REPORT_FILE??"/evidence/report.json",JSON.stringify(report,null,2)); console.log(JSON.stringify(report)); process.exitCode=Number(report.failed)>0?2:0; }
  catch (error) { console.error(JSON.stringify(failureDiagnostic(error))); process.exitCode=3; }
}
