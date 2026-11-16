import { writeFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { validatePlan, type Arrival } from "./run.ts";

/** Conservative independent reference phases, never a reconstructed joint peak. */
export function referencePlan(releaseBatch:string, completionRate:number, sampledActive:number) {
  if(!releaseBatch || !Number.isFinite(completionRate)||completionRate<=0 || !Number.isSafeInteger(sampledActive)||sampledActive<=0)throw new Error("explicit measured lower bounds required");
  const arrivals:Arrival[]=[];
  const base={protocol:"responses" as const,scenario:"normal" as const,request_bytes:0,response_bytes:40960,chunk_delay_ms:1000,client_read_delay_ms:0,expected_status:200};
  for(let index=0;index<sampledActive;index++)arrivals.push({...base,at_ms:0,session:`concurrency-reference-${index}`,stream:true});
  const count=Math.ceil(completionRate*60);
  for(let index=0;index<count;index++)arrivals.push({...base,at_ms:30_000+Math.floor(index*60_000/count),session:`completion-reference-${index}`,stream:false,response_bytes:2,chunk_delay_ms:0});
  arrivals.push({...base,at_ms:95_000,session:"tool-contract",protocol:"chat",scenario:"tool-session",stream:false,response_bytes:0,chunk_delay_ms:0});
  arrivals.push({...base,at_ms:98_000,session:"slow-read-contract",scenario:"slow-read",stream:false,response_bytes:2,chunk_delay_ms:0});
  arrivals.push({...base,at_ms:101_000,session:"large-body-contract",protocol:"chat",stream:true,response_bytes:16*1024*1024,chunk_delay_ms:0});
  arrivals.push({...base,at_ms:104_000,session:"upstream-error-contract",scenario:"upstream-error",stream:false,response_bytes:0,chunk_delay_ms:0,expected_status:503});
  return {...validatePlan({release_batch:releaseBatch,window_ms:110_000,drain_timeout_ms:120_000,baseline:null,arrivals}),reference_evidence:{kind:"independent sampled lower bounds; not full historical peak",completion_rate_per_second:completionRate,sampled_active:sampledActive,observation_window:"48h",sampling:"one-minute subquery",unknown:["offered one-second peak","exact concurrent peak","body distributions","stream duration distribution","protocol/tool joint mix"]}};
}
if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href){
  const plan=referencePlan(process.env.MTC_RELEASE_BATCH??"",Number(process.env.MTC_OBSERVED_COMPLETION_RATE),Number(process.env.MTC_OBSERVED_ACTIVE));
  writeFileSync(process.env.MTC_PEAK_PLAN_FILE??"plan.json",JSON.stringify(plan,null,2));
}
