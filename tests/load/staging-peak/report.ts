import { readFileSync, writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { createHash } from "node:crypto";
import { validateBaseline, baselineIdentity } from "./baseline.ts";

const requiredChecks=["financial_exactly_once","semantic_payloads_verified","archive_debt_drained","cancelled_requests_released","pool_wait_recovered","io_recovered","memory_recovered","mock_attempt_identity_verified","latency_paired_comparison_accepted"] as const;
export function qualify(load:any,observer:any,baseline:unknown) {
  const missing:string[]=[],failures:string[]=[];
  if(!load?.run_id || load.started!==load.offered || load.unissued!==0)failures.push("generator_dropped_arrivals");
  if(load?.failed>0)failures.push("gateway_workload_failures");
  if(!observer || observer.run_id!==load?.run_id || observer.source_sha!==load?.source_sha)missing.push("matching_run_observer");
  for(const check of requiredChecks){const value=observer?.checks?.[check];if(value===false)failures.push(check);else if(value!==true)missing.push(check);}
  let baselineHash:string|null=null;
  try {
    const historical=validateBaseline(baseline);baselineHash=baselineIdentity(historical);
    for(const dimension of ["offered_1s","offered_60s","concurrency_intervals","request_bytes","response_bytes","stream_durations","protocol_tools_mix"]){if((historical as any).coverage?.[dimension]!==true)missing.push(`historical_${dimension}`);}
    for(const window of historical.windows){
      const starts=(load.requests??[]).map((row:any)=>Number(row.started_at_ms)).sort((a:number,b:number)=>a-b);let best=0,left=0;
      for(let right=0;right<starts.length;right++){while(starts[right]-starts[left]>=window.seconds*1000)left++;best=Math.max(best,right-left+1);}
      if(best<window.requests)failures.push(`admitted_${window.seconds}s_below_baseline`);
      if(load.peak_active<window.max_in_flight)failures.push("admitted_concurrency_below_baseline");
    }
    if(!(historical.windows.some(row=>row.seconds===1)&&historical.windows.some(row=>row.seconds===60)))missing.push("historical_1s_and_60s_windows");
    if(observer?.checks?.historical_joint_payload_and_duration_coverage!==true)missing.push("historical_joint_payload_and_duration_coverage");
  }catch{missing.push("complete_historical_baseline");}
  return {schema_version:1,run_id:load?.run_id??null,source_sha:load?.source_sha??null,qualification:failures.length?"FAILED":missing.length?"NOT_EVALUATED":"PASS",failures:[...new Set(failures)],missing_evidence:[...new Set(missing)],baseline_sha256:baselineHash,load_summary:{offered:load?.offered,started:load?.started,completed:load?.completed,failed:load?.failed,peak_active:load?.peak_active,latency_ms:load?.latency_ms},observer_sha256:observer?createHash("sha256").update(JSON.stringify(observer)).digest("hex"):null};
}
if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href){
  const read=(name:string)=>process.env[name]?JSON.parse(readFileSync(process.env[name]!,"utf8")):null;
  const report=qualify(read("MTC_LOAD_REPORT_FILE"),read("MTC_OBSERVER_REPORT_FILE"),read("MTC_BASELINE_FILE"));
  const text=JSON.stringify(report,null,2);if(process.env.MTC_QUALIFICATION_REPORT_FILE)writeFileSync(process.env.MTC_QUALIFICATION_REPORT_FILE,text);console.log(text);process.exitCode=report.qualification==="PASS"?0:report.qualification==="FAILED"?2:3;
}
