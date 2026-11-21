/** Reuse only a completely matching isolated fixture; partial/conflicting setup stays visible. */
export async function reuseFixture(control: URL | string, token: string, tenant: string, runId: string, upstreamBase: URL | string, scenarios: string[], concurrency: number, request: typeof fetch = fetch) {
  const headers={authorization:`Bearer ${token}`};
  const get=async(path:string)=>{const r=await request(new URL(path,control),{headers,signal:AbortSignal.timeout(30_000)});if(!r.ok)throw new Error("fixture lookup failed");return await r.json() as any;};
  const list=async(path:string,extra:Record<string,string>={})=>{
    const rows:any[]=[];let before:any;
    for(;;){const q=new URLSearchParams({tenant_external_id:tenant,limit:"100",...extra});if(before){q.set("before_created_at",String(before.created_at));q.set("before_id",String(before.id??before.key_id));}const page=await get(`${path}?${q}`);if(!Array.isArray(page))throw new Error("fixture directory malformed");rows.push(...page);if(page.length<100)return rows;const tail=page.at(-1);if(before?.created_at===tail.created_at&&(before.id??before.key_id)===(tail.id??tail.key_id))throw new Error("fixture directory did not advance");before=tail;}
  };
  const upstreams=await list("/internal/v1/upstreams");
  const names=scenarios.map(s=>`staging-peak-${runId}-${s}`);
  const existing=upstreams.filter(row=>names.includes(row.name));
  if(!existing.length)return null;
  const routes=await list("/internal/v1/model-routes");const selected:Record<string,string>={};
  for(const scenario of scenarios){
    const model=`staging-peak-${runId}-${scenario}`;
    const matches=existing.filter(row=>row.name===model);
    if(matches.length!==1)throw new Error("fixture upstream incomplete or ambiguous");
    const u=matches[0];
    if(u.tenant_external_id!==tenant||u.driver!=="http-json"||u.auth_kind!=="none"||u.status!=="active"||u.has_proxy||u.config?.base_url!==new URL(`/scenario/${scenario}`,upstreamBase).href||u.config?.network_scope!=="private")throw new Error("fixture upstream differs from isolated mock");
    const found=routes.filter(row=>row.public_model===model);
    if(found.length!==1)throw new Error("fixture route incomplete or ambiguous");
    const r=found[0];
    if(r.tenant_external_id!==tenant||!r.enabled||r.protocol!=="openai"||r.upstream_model!==model||r.upstream_account_id!==u.id||r.priority!==0||(r.upstream_account_ids??[u.id]).some((id:string)=>id!==u.id)||(r.included_provider_group_ids??[]).length||(r.route_group_ids??[]).length)throw new Error("fixture route differs from isolated mock");
    selected[scenario]=String(r.id);
  }
  const keys=(await list("/internal/v1/keys",{principal_external_id:`staging-peak-${runId}`,status:"active"})).filter(row=>row.alias===`Isolated ${runId}`);
  if(keys.length!==1)throw new Error("fixture key incomplete or ambiguous");
  const k=keys[0];if(k.tenant_external_id!==tenant||k.currency!=="USD"||!k.credential_copy_available||k.policy?.max_concurrency<concurrency||k.policy?.enforcement_mode!=="metered_unlimited")throw new Error("fixture key differs from workload");
  const binding=await get(`/internal/v1/keys/${k.key_id}/routing?${new URLSearchParams({tenant_external_id:tenant})}`);
  const expected=Object.values(selected).sort();
  if((binding.route_group_ids??[]).length||JSON.stringify([...(binding.effective_route_ids??[])].sort())!==JSON.stringify(expected)||JSON.stringify([...(binding.route_ids??[])].sort())!==JSON.stringify(expected))throw new Error("fixture key routing differs from isolated mock");
  const copied=await request(new URL(`/internal/v1/keys/${k.key_id}/copy`,control),{method:"POST",headers,signal:AbortSignal.timeout(30_000)});
  if(!copied.ok)throw new Error("fixture key copy failed");const credential=await copied.json() as any;
  if(credential.key_id!==k.key_id||typeof credential.key!=="string")throw new Error("fixture key copy identity mismatch");
  return {routes:selected,key:credential.key};
}
