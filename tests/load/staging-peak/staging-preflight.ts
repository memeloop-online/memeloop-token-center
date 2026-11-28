/** Fail-closed read-only identity check. Does not create load or mutate resources. */
import { spawnSync } from "node:child_process";
import { readFileSync } from "node:fs";
import { isIP } from "node:net";
import { pathToFileURL } from "node:url";
import { isDeepStrictEqual } from "node:util";

type Resource = Record<string, any>;
type ReadResource = (...args: string[]) => Resource;
function requireIdentity(ok: unknown, message: string): asserts ok {
  if (!ok) throw new Error(message);
}
function identity(value: unknown): value is string {
  return typeof value === "string" && value.length > 0;
}
function address(value: string): { family: number; canonical: string } {
  const family = isIP(value);
  requireIdentity(family !== 0, "invalid IP address");
  if (family === 4) return { family, canonical: value };
  const [host, scope] = value.split("%");
  const canonical = new URL(`http://[${host}]/`).hostname;
  return { family, canonical: scope === undefined ? canonical : `${canonical}%${scope}` };
}
export function verifyEndpointSlice(item: Resource, service: Resource, namespace: string, podByName: (name: string) => Resource): number {
  requireIdentity(identity(service.metadata.uid), "missing Service UID");
  requireIdentity(item.metadata.labels?.["endpointslice.kubernetes.io/managed-by"] === "endpointslice-controller.k8s.io", "untrusted EndpointSlice controller");
  requireIdentity(item.metadata.ownerReferences?.some((owner: Resource) => owner.kind === "Service" && owner.uid === service.metadata.uid && owner.controller === true), "foreign service owner");
  const ports = item.ports ?? [];
  requireIdentity(ports.length === 1 && ports[0].port === 8080 && (ports[0].protocol === undefined ? "TCP" : ports[0].protocol) === "TCP", "unexpected endpoint port");
  const family = ({ IPv4: 4, IPv6: 6 } as Record<string, number>)[item.addressType];
  requireIdentity(family !== undefined, "unsupported address type");
  let count = 0;
  for (const endpoint of item.endpoints) {
    const ref = endpoint.targetRef ?? {};
    requireIdentity(ref.kind === "Pod" && (ref.namespace === undefined ? namespace : ref.namespace) === namespace && identity(ref.uid) && identity(ref.name), "endpoint is not a namespaced pod");
    const pod = podByName(ref.name);
    requireIdentity(pod.metadata.uid === ref.uid, "pod uid mismatch");
    requireIdentity(Object.entries(service.spec.selector).every(([key, value]) => pod.metadata.labels?.[key] === value), "pod selector mismatch");
    const actual = new Set((pod.status.podIPs ?? []).map((entry: Resource) => address(entry.ip).canonical));
    if (pod.status.podIP) actual.add(address(pod.status.podIP).canonical);
    const addresses = endpoint.addresses ?? [];
    requireIdentity(addresses.length > 0, "empty endpoint addresses");
    for (const value of addresses) {
      const parsed = address(value);
      requireIdentity(parsed.family === family && actual.has(parsed.canonical), "endpoint address does not belong to referenced pod");
    }
    count += 1;
  }
  return count;
}
export function verifyStagingIdentities(meta: Resource, get: ReadResource): Record<string, number> {
  requireIdentity(identity(meta.context) && identity(meta.clusterUid) && identity(meta.namespaceUid), "missing context or identity UID");
  const namespace = "memeloop-token-center-staging";
  requireIdentity(get("get", "namespace", "kube-system").metadata.uid === meta.clusterUid, "cluster UID mismatch");
  requireIdentity(get("get", "namespace", namespace).metadata.uid === meta.namespaceUid, "namespace UID mismatch");
  const counts: Record<string, number> = {};
  for (const role of ["gateway", "control"]) {
    const name = `mtc-staging-${role}`;
    const service = get("-n", namespace, "get", "service", name);
    requireIdentity(service.spec.type !== "ExternalName" && isDeepStrictEqual(service.spec.selector, meta[`${role}Selector`]) && Object.keys(service.spec.selector ?? {}).length > 0, "service selector mismatch");
    const ports = service.spec.ports ?? [];
    requireIdentity(ports.length === 1 && ports[0].port === 8080 && (ports[0].protocol === undefined ? "TCP" : ports[0].protocol) === "TCP", "unexpected service port");
    const slices = get("-n", namespace, "get", "endpointslices", "-l", `kubernetes.io/service-name=${name}`).items;
    requireIdentity(Array.isArray(slices) && slices.length > 0, "no EndpointSlices");
    const count = slices.reduce((sum: number, item: Resource) => sum + verifyEndpointSlice(item, service, namespace, (pod) => get("-n", namespace, "get", "pod", pod)), 0);
    requireIdentity(count > 0, "no endpoint pods");
    counts[role] = count;
  }
  return counts;
}
export function main(metadataPath: string): void {
  const meta = JSON.parse(readFileSync(metadataPath, "utf8"));
  const counts = verifyStagingIdentities(meta, (...args) => {
    const result = spawnSync("kubectl", ["--context", meta.context, ...args, "-o", "json"], { encoding: "utf8", shell: false });
    if (result.error || result.status !== 0) throw new Error("read-only identity lookup failed");
    return JSON.parse(result.stdout);
  });
  for (const [role, count] of Object.entries(counts)) console.log(role, "verified_endpoints", count);
  console.log("staging identities verified; no load started");
}
if (import.meta.url === pathToFileURL(process.argv[1] ?? "").href) main(process.argv[2]!);
