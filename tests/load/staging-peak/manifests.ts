import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { validatePlan, type Plan } from "./run.ts";

export function manifests(runId:string,image:string,sourceSha:string,plan:Plan, target: "test" | "staging" = "test", staging?: {secretName:string; gatewaySelector:Record<string,string>; controlSelector:Record<string,string>}) {
  if(!/^[a-z0-9-]{1,40}$/.test(runId)||!/@sha256:[a-f0-9]{64}$/.test(image)||!/^[a-f0-9]{40}$/.test(sourceSha)) throw new Error("run identity, immutable image digest and source SHA required");
  validatePlan(plan);
  if (target !== "test" && target !== "staging") throw new Error("explicit isolated target required");
  if (target === "staging" && (!staging?.secretName || !Object.keys(staging.gatewaySelector ?? {}).length || !Object.keys(staging.controlSelector ?? {}).length)) throw new Error("verified staging secret and pod selectors required");
  const namespace=target === "staging" ? "memeloop-token-center-staging" : "memeloop-token-center-test", name=`mtc-peak-${runId}`, labels={"app.kubernetes.io/part-of":"mtc-staging-peak","mtc.memeloop.ai/peak-run":runId};
  const meta=(suffix:string)=>({name:`${name}-${suffix}`,namespace,labels});
  const gateway=target === "staging" ? staging!.gatewaySelector : {"app.kubernetes.io/instance":namespace,"app.kubernetes.io/component":"gateway"};
  const control=target === "staging" ? staging!.controlSelector : {"app.kubernetes.io/instance":namespace,"app.kubernetes.io/component":"control"};
  const securityContext={runAsNonRoot:true,runAsUser:1000,runAsGroup:1000,fsGroup:1000,seccompProfile:{type:"RuntimeDefault"}};
  const containerSecurity={allowPrivilegeEscalation:false,capabilities:{drop:["ALL"]},readOnlyRootFilesystem:true};
  const env=[{name:"MTC_PEAK_RUN_ID",value:runId},{name:"MTC_HARNESS_SOURCE_SHA",value:sourceSha}];
  return {apiVersion:"v1",kind:"List",items:[
    {apiVersion:"v1",kind:"ConfigMap",metadata:meta("plan"),data:{"plan.json":JSON.stringify(plan)}},
    ...(target === "test" ? [{apiVersion:"v1",kind:"Service",metadata:meta("mock"),spec:{selector:{...labels,"app.kubernetes.io/component":"mock"},ports:[{port:8080,targetPort:8080}]}}] : []),
    {apiVersion:"apps/v1",kind:"Deployment",metadata:meta("mock"),spec:{replicas:1,selector:{matchLabels:{...labels,"app.kubernetes.io/component":"mock"}},template:{metadata:{labels:{...labels,"app.kubernetes.io/component":"mock"}},spec:{automountServiceAccountToken:false,securityContext,containers:[{name:"mock",image,env,securityContext:containerSecurity,ports:[{containerPort:8080}],readinessProbe:{httpGet:{path:"/healthz",port:8080}},resources:{requests:{cpu:"250m",memory:"256Mi"},limits:{cpu:"2",memory:"1Gi"}}}]}}}},
    {apiVersion:"batch/v1",kind:"Job",metadata:meta("load"),spec:{backoffLimit:0,activeDeadlineSeconds:Math.ceil((plan.window_ms+plan.drain_timeout_ms)/1000)+300,template:{metadata:{labels:{...labels,"app.kubernetes.io/component":"load"}},spec:{restartPolicy:"Never",automountServiceAccountToken:false,securityContext,containers:[{name:"load",image,command:["node","tests/load/staging-peak/run.ts"],securityContext:containerSecurity,env:[...env,{name:"POD_NAMESPACE",valueFrom:{fieldRef:{fieldPath:"metadata.namespace"}}},{name:"MTC_TEST_SERVICE_TOKEN",valueFrom:{secretKeyRef:{name:target === "staging" ? staging!.secretName : "memeloop-token-center-test-secrets",key:"service-token"}}}],resources:{requests:{cpu:"500m",memory:"512Mi"},limits:{cpu:"2",memory:"2Gi"}},volumeMounts:[{name:"plan",mountPath:"/plan",readOnly:true},{name:"evidence",mountPath:"/evidence"}]}],volumes:[{name:"plan",configMap:{name:`${name}-plan`}},{name:"evidence",emptyDir:{sizeLimit:"256Mi"}}]}}}},
    {apiVersion:"networking.k8s.io/v1",kind:"NetworkPolicy",metadata:meta("mock-network"),spec:{podSelector:{matchLabels:{...labels,"app.kubernetes.io/component":"mock"}},policyTypes:["Ingress","Egress"],ingress:[{from:[{podSelector:{matchLabels:gateway}},{podSelector:{matchLabels:{...labels,"app.kubernetes.io/component":"load"}}}],ports:[{port:8080,protocol:"TCP"}]}],egress:[]}},
    {apiVersion:"networking.k8s.io/v1",kind:"NetworkPolicy",metadata:meta("load-network"),spec:{podSelector:{matchLabels:{...labels,"app.kubernetes.io/component":"load"}},policyTypes:["Ingress","Egress"],ingress:[],egress:[{to:[{podSelector:{matchLabels:gateway}},{podSelector:{matchLabels:control}},{podSelector:{matchLabels:{...labels,"app.kubernetes.io/component":"mock"}}}],ports:[{port:8080,protocol:"TCP"}]},{to:[{namespaceSelector:{matchLabels:{"kubernetes.io/metadata.name":"kube-system"}},podSelector:{matchLabels:{"k8s-app":"kube-dns"}}}],ports:[{port:53,protocol:"UDP"},{port:53,protocol:"TCP"}]}]}}
  ]};
}
if(process.argv[1]&&import.meta.url===pathToFileURL(process.argv[1]).href) {
  const plan=validatePlan(JSON.parse(readFileSync(process.env.MTC_PEAK_PLAN_FILE??"plan.json","utf8")));
  const target=process.env.MTC_PEAK_TARGET;
  if (target !== "test" && target !== "staging") throw new Error("MTC_PEAK_TARGET must explicitly select test or staging");
  console.log(JSON.stringify(manifests(process.env.MTC_PEAK_RUN_ID??"",process.env.MTC_HARNESS_IMAGE??"",process.env.MTC_HARNESS_SOURCE_SHA??"",plan,target,target === "staging" ? JSON.parse(readFileSync(process.env.MTC_PEAK_STAGE_METADATA_FILE??"", "utf8")) : undefined),null,2));
}
