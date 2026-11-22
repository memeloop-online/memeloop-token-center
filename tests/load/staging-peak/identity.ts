import { createHash } from "node:crypto";
export type Candidate = Record<"gateway"|"control"|"worker",{source_sha:string;image_digest:string}>;
export function artifactHash(value:unknown):string {
  const ordered=(input:any):any=>Array.isArray(input)?input.map(ordered):input&&typeof input==="object"?Object.fromEntries(Object.keys(input).sort().filter(key=>input[key]!==undefined).map(key=>[key,ordered(input[key])])):input;
  return createHash("sha256").update(JSON.stringify(ordered(value??null))).digest("hex");
}
export function candidateIdentity(value:any):Candidate {
  const result={} as Candidate;
  for(const role of ["gateway","control","worker"] as const){const item=value?.[role];if(!/^[a-f0-9]{40}$/.test(item?.source_sha??"")||!/^sha256:[a-f0-9]{64}$/.test(item?.image_digest??""))throw new Error("actual role source and immutable image digest required");result[role]={source_sha:item.source_sha,image_digest:item.image_digest};}
  return result;
}
