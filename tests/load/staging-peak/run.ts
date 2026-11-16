import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { setTimeout as delay } from "node:timers/promises";
import { pathToFileURL } from "node:url";
import { validateBaseline, baselineIdentity } from "./baseline.ts";

export type Arrival = { at_ms: number; scenario: "normal" | "slow-read" | "upstream-error" | "disconnect"; session: string; stream: boolean; request_bytes: number; response_bytes: number; chunk_delay_ms: number; client_read_delay_ms: number; expected_status: number };
export type Plan = { release_batch: string; window_ms: number; drain_timeout_ms: number; arrivals: Arrival[]; baseline: unknown };
export function validatePlan(value: unknown): Plan {
  const plan = value as Plan;
  if (!plan?.release_batch || !Number.isSafeInteger(plan.window_ms) || plan.window_ms <= 0 || !Number.isSafeInteger(plan.drain_timeout_ms) || plan.drain_timeout_ms <= 0 || !Array.isArray(plan.arrivals) || !plan.arrivals.length) throw new Error("explicit release batch, arrival window and drain period required");
  let previous = -1;
  for (const row of plan.arrivals) {
    if (!Number.isSafeInteger(row.at_ms) || row.at_ms < previous || row.at_ms >= plan.window_ms || !["normal", "slow-read", "upstream-error", "disconnect"].includes(row.scenario) || !row.session || typeof row.stream !== "boolean") throw new Error("invalid ordered arrival or scenario");
    for (const number of [row.request_bytes, row.response_bytes, row.chunk_delay_ms, row.client_read_delay_ms]) if (!Number.isSafeInteger(number) || number < 0) throw new Error("invalid measured workload size or delay");
    if (!Number.isInteger(row.expected_status) || row.expected_status < 100 || row.expected_status > 599) throw new Error("expected gateway result required");
    if (row.stream && row.response_bytes < 4096) throw new Error("SSE workload must include protocol framing");
    previous = row.at_ms;
  }
  return plan;
}
const percentile = (values: number[], p: number) => values.length ? [...values].sort((a,b) => a-b)[Math.ceil(values.length*p)-1] : null;

export async function run(plan: Plan, env = process.env): Promise<Record<string, unknown>> {
  validatePlan(plan);
  if (env.POD_NAMESPACE !== "memeloop-token-center-test") throw new Error("only the isolated TEST namespace is supported");
  const runId = env.MTC_PEAK_RUN_ID;
  if (!runId || !/^[a-z0-9-]{1,40}$/.test(runId)) throw new Error("DNS-safe explicit run identity required");
  const gateway = new URL(env.MTC_TEST_GATEWAY_URL ?? "http://memeloop-token-center-test-gateway:8080");
  const control = new URL(env.MTC_TEST_CONTROL_URL ?? "http://memeloop-token-center-test-control:8080");
  const mock = new URL(env.MTC_TEST_MOCK_URL ?? `http://mtc-peak-${runId}-mock:8080`);
  for (const [url, host] of [[gateway,"memeloop-token-center-test-gateway"],[control,"memeloop-token-center-test-control"],[mock,`mtc-peak-${runId}-mock`]] as const) if (url.protocol !== "http:" || url.hostname !== host || url.port !== "8080" || url.pathname !== "/") throw new Error("TEST-only service identity mismatch");
  const token = env.MTC_TEST_SERVICE_TOKEN;
  if (!token) throw new Error("TEST service token secret reference required");
  const call = async (path: string, payload: unknown) => {
    const response = await fetch(new URL(path, control), { method: "POST", headers: { authorization: `Bearer ${token}`, "content-type":"application/json" }, body: JSON.stringify(payload), signal: AbortSignal.timeout(30_000) });
    if (!response.ok) throw new Error(`isolated control setup HTTP ${response.status}`);
    return await response.json() as Record<string, any>;
  };
  const tenant = `staging-peak-${runId}`, models: Record<string,string> = {}, routes: string[] = [];
  for (const scenario of new Set(plan.arrivals.map(row => row.scenario))) {
    const model = `staging-peak-${runId}-${scenario}`;
    const upstream = await call("/internal/v1/upstreams", { tenant_external_id:tenant, name:model, driver:"http-json", config:{ base_url:new URL(`/scenario/${scenario}`,mock).href }, credential:{ type:"none" } });
    const route = await call("/internal/v1/model-routes", { tenant_external_id:tenant, public_model:model, upstream_account_id:upstream.id, upstream_model:model, protocol:"openai", priority:0, custom_model_confirmed:true });
    await call(`/internal/v1/prices/USD/${model}`, { input_per_million:"0.01", output_per_million:"0.02" });
    models[scenario]=model; routes.push(String(route.id));
  }
  const issued = await call("/internal/v1/keys", { tenant_external_id:tenant, principal_external_id:`staging-peak-${runId}`, alias:`Isolated ${runId}`, currency:"USD", initial_balance:"100000", route_ids:routes, policy:{requests_per_minute:Math.max(1,plan.arrivals.length*60), tokens_per_minute:1_000_000_000, max_concurrency:plan.arrivals.length, enforcement_mode:"metered_unlimited", daily_budget:null, weekly_budget:null, lifetime_budget:null} });
  const key = String(issued.key);
  const requests: Record<string,unknown>[] = [];
  let active=0, peakActive=0;
  const start=performance.now();
  const tasks: Promise<void>[]=[];
  for (const [index, arrival] of plan.arrivals.entries()) {
    const remaining=start+arrival.at_ms-performance.now(); if (remaining>0) await delay(remaining);
    const lag=performance.now()-start-arrival.at_ms;
    active++; peakActive=Math.max(peakActive,active);
    tasks.push((async () => {
      const began=performance.now(), session=`${runId}:${arrival.session}`;
      let status=0, bytes=0, error:string|undefined, hash=createHash("sha256"), tail="";
      try {
        const payload: Record<string,unknown>={ model:models[arrival.scenario], messages:[{role:"user",content:"synthetic peak replay"+"x".repeat(arrival.request_bytes)}], metadata:{session_id:session}, stream:arrival.stream, max_tokens:1 };
        if (arrival.stream) payload.benchmark={mode:"stream",bytes:arrival.response_bytes,chunk_bytes:4096,delay_ms:arrival.chunk_delay_ms};
        const response=await fetch(new URL("/v1/chat/completions",gateway), {method:"POST",headers:{authorization:`Bearer ${key}`,"content-type":"application/json","x-mtc-conversation-id":session},body:JSON.stringify(payload),signal:AbortSignal.timeout(plan.window_ms+plan.drain_timeout_ms)});
        status=response.status;
        if (response.body) for await (const chunk of response.body) { bytes+=chunk.length; hash.update(chunk); tail=(tail+Buffer.from(chunk).toString("utf8")).slice(-4096); if(arrival.client_read_delay_ms) await delay(arrival.client_read_delay_ms); }
        if(status!==arrival.expected_status) error="unexpected_gateway_status";
        if(arrival.stream && status===200 && !tail.includes("data: [DONE]")) error="missing_stream_terminal";
      } catch { error="transport_or_timeout"; }
      finally { active--; requests.push({index,session,scenario:arrival.scenario,offered_at_ms:arrival.at_ms,started_at_ms:began-start,generator_lag_ms:lag,status,response_bytes:bytes,response_sha256:hash.digest("hex"),latency_ms:performance.now()-began,error}); }
    })());
  }
  await Promise.all(tasks);
  if(performance.now()-start<plan.window_ms) await delay(plan.window_ms-(performance.now()-start));
  let baselineHash:string|undefined, baselineError:string|undefined;
  try { baselineHash=baselineIdentity(validateBaseline(plan.baseline)); } catch { baselineError="historical_peak_coverage_incomplete"; }
  const latencies=requests.map(row=>Number(row.latency_ms));
  return {schema_version:1,run_id:runId,release_batch:plan.release_batch,source_sha:env.MTC_HARNESS_SOURCE_SHA??null,tenant,offered:plan.arrivals.length,started:requests.length,completed:requests.filter(row=>!row.error).length,failed:requests.filter(row=>row.error).length,unissued:plan.arrivals.length-requests.length,peak_active:peakActive,latency_ms:{p50:percentile(latencies,.5),p95:percentile(latencies,.95),p99:percentile(latencies,.99),max:Math.max(...latencies)},baseline_sha256:baselineHash,baseline_error:baselineError,qualification:"NOT_EVALUATED",missing_evidence:["complete historical multidimensional peak","financial exactly-once assertions","semantic/archive API payload verification","PG poolwait/IO/locks and memory recovery","archive debt drain confirmation"],requests};
}
if(process.argv[1] && import.meta.url===pathToFileURL(process.argv[1]).href) {
  try { const report=await run(validatePlan(JSON.parse(readFileSync(process.env.MTC_PEAK_PLAN_FILE??"/plan/plan.json","utf8")))); writeFileSync(process.env.MTC_PEAK_REPORT_FILE??"/evidence/report.json",JSON.stringify(report,null,2)); console.log(JSON.stringify(report)); process.exitCode=Number(report.failed)>0?2:0; }
  catch { console.error("staging peak harness failed; credentials and response bodies suppressed"); process.exitCode=3; }
}
