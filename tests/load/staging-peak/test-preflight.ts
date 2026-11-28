import assert from "node:assert/strict";
import test from "node:test";
import { verifyEndpointSlice, verifyStagingIdentities } from "./staging-preflight.ts";
function fixture() {
  const service = { metadata: { uid: "svc" }, spec: { selector: { app: "gateway" } } };
  const pod = { metadata: { uid: "pod", labels: { app: "gateway" } }, status: { podIP: "10.0.0.1", podIPs: [{ ip: "10.0.0.1" }, { ip: "fd00::1" }] } };
  const slice = { addressType: "IPv4", metadata: { labels: { "endpointslice.kubernetes.io/managed-by": "endpointslice-controller.k8s.io" }, ownerReferences: [{ kind: "Service", uid: "svc", controller: true }] }, ports: [{ port: 8080, protocol: "TCP" }], endpoints: [{ addresses: ["10.0.0.1"], targetRef: { kind: "Pod", namespace: "stage", name: "p", uid: "pod" } }] };
  return { service, pod, slice, check: () => verifyEndpointSlice(slice, service, "stage", () => pod) };
}
test("accepts pod IPv4 and canonical IPv6", () => {
  const f = fixture(); assert.equal(f.check(), 1);
  f.slice.addressType = "IPv6"; f.slice.endpoints[0]!.addresses = ["fd00:0:0:0:0:0:0:1"]; assert.equal(f.check(), 1);
});
for (const [name, mutate] of Object.entries({
  "foreign IP": (f: any) => { f.slice.endpoints[0].addresses = ["10.0.0.9"]; },
  "address family mismatch": (f: any) => { f.slice.endpoints[0].addresses = ["fd00::1"]; },
  "manual controller": (f: any) => { f.slice.metadata.labels["endpointslice.kubernetes.io/managed-by"] = "manual"; },
  "wrong port": (f: any) => { f.slice.ports[0].port = 9999; },
  "both Service UIDs missing": (f: any) => { delete f.service.metadata.uid; delete f.slice.metadata.ownerReferences[0].uid; },
  "both Service UIDs empty": (f: any) => { f.service.metadata.uid = ""; f.slice.metadata.ownerReferences[0].uid = ""; },
  "both Pod UIDs missing": (f: any) => { delete f.pod.metadata.uid; delete f.slice.endpoints[0].targetRef.uid; },
  "empty Pod name": (f: any) => { f.slice.endpoints[0].targetRef.name = ""; },
  "foreign Service UID": (f: any) => { f.slice.metadata.ownerReferences[0].uid = "foreign"; },
  "noncontroller owner": (f: any) => { f.slice.metadata.ownerReferences[0].controller = false; },
  "wrong protocol": (f: any) => { f.slice.ports[0].protocol = "UDP"; },
  "null protocol": (f: any) => { f.slice.ports[0].protocol = null; },
  "null target namespace": (f: any) => { f.slice.endpoints[0].targetRef.namespace = null; },
  "unsupported address type": (f: any) => { f.slice.addressType = "FQDN"; },
  "foreign namespace": (f: any) => { f.slice.endpoints[0].targetRef.namespace = "other"; },
  "nonPod target": (f: any) => { f.slice.endpoints[0].targetRef.kind = "Service"; },
  "Pod UID mismatch": (f: any) => { f.pod.metadata.uid = "other"; },
  "Pod selector mismatch": (f: any) => { f.pod.metadata.labels.app = "other"; },
  "empty addresses": (f: any) => { f.slice.endpoints[0].addresses = []; },
  "invalid IP": (f: any) => { f.slice.endpoints[0].addresses = ["not-an-ip"]; },
})) test(`rejects ${name}`, () => { const f = fixture(); mutate(f); assert.throws(f.check); });
function clusterFixture() {
  const f = fixture();
  f.slice.endpoints[0]!.targetRef.namespace = "memeloop-token-center-staging";
  const meta = { context: "fixture-context", clusterUid: "cluster", namespaceUid: "namespace", gatewaySelector: { app: "gateway" }, controlSelector: { app: "gateway" } };
  const services: any = { ...f.service, spec: { ...f.service.spec, ports: [{ port: 8080, protocol: "TCP" }] } };
  const resources = { slices: [f.slice] };
  const calls: string[][] = [];
  const get = (...args: string[]) => {
    calls.push(args);
    const verb = args[0] === "-n" ? args[2] : args[0];
    assert.equal(verb, "get", "every resource operation must be read-only");
    if (args[1] === "namespace") return { metadata: { uid: args[2] === "kube-system" ? "cluster" : "namespace" } };
    if (args[3] === "service") return services;
    if (args[3] === "endpointslices") return { items: resources.slices };
    if (args[3] === "pod") return f.pod;
    throw new Error("unexpected resource lookup");
  };
  return { ...f, meta, services, resources, calls, check: () => verifyStagingIdentities(meta, get) };
}
test("verifies both services with read-only lookups", () => { const f = clusterFixture(); assert.deepEqual(f.check(), { gateway: 1, control: 1 }); assert.equal(f.calls.length, 8); });
for (const [name, mutate] of Object.entries({
  "missing cluster UID": (f: any) => { delete f.meta.clusterUid; },
  "empty namespace UID": (f: any) => { f.meta.namespaceUid = ""; },
  "missing context": (f: any) => { delete f.meta.context; },
  "null context": (f: any) => { f.meta.context = null; },
  "cluster UID": (f: any) => { f.meta.clusterUid = "wrong"; },
  "namespace UID": (f: any) => { f.meta.namespaceUid = "wrong"; },
  "service selector": (f: any) => { f.meta.gatewaySelector = { app: "other" }; },
  "empty selector": (f: any) => { f.meta.gatewaySelector = {}; f.services.spec.selector = {}; },
  "ExternalName": (f: any) => { f.services.spec.type = "ExternalName"; },
  "service port": (f: any) => { f.services.spec.ports[0].port = 9999; },
  "empty EndpointSlices": (f: any) => { f.resources.slices = []; },
  "empty endpoint pods": (f: any) => { f.slice.endpoints = []; },
})) test(`rejects ${name} before any load`, () => { const f = clusterFixture(); mutate(f); assert.throws(f.check); });
