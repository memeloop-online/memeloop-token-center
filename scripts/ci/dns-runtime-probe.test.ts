import assert from "node:assert/strict";
import test from "node:test";
import { dnsReply, HOST, CANARY } from "./dns-runtime-probe.ts";
import { renderJob, verifyLogs } from "./dns-runtime-job.ts";

function question(host = HOST, type = 1) {
  const header = Buffer.alloc(12);
  header.writeUInt16BE(123, 0);
  header.writeUInt16BE(1, 4);
  const labels = host.split(".").flatMap(label => [Buffer.from([label.length]), Buffer.from(label)]);
  const tail = Buffer.from([0, 0, type, 0, 1]);
  return Buffer.concat([header, ...labels, tail]);
}

test("fixed DNS fixture distinguishes A, AAAA, NXDOMAIN and blackhole", () => {
  const answer = dnsReply(question(), "healthy")!;
  assert.equal(answer.readUInt16BE(6), 1);
  assert.equal(answer.subarray(-4).toString("hex"), "7f000001");
  assert.equal(dnsReply(question(HOST, 28), "healthy")!.readUInt16BE(6), 0);
  assert.equal(dnsReply(question(), "nxdomain")!.readUInt16BE(2) & 15, 3);
  assert.equal(dnsReply(question("external.example"), "healthy")!.readUInt16BE(2) & 15, 3);
  assert.equal(dnsReply(question(), "drop"), undefined);
  assert.equal(dnsReply(Buffer.alloc(2), "healthy"), undefined);
});

test("job is isolated with digest pins, ephemeral data, and synthetic credentials", () => {
  const digest = "a".repeat(64);
  const rendered = renderJob(
    `ghcr.io/memeloop-online/memeloop-token-center@sha256:${digest}`,
    `node:22@sha256:${digest}`,
  );
  assert.ok(rendered.items.every(item => item.metadata.namespace === "memeloop-token-center-cloud-test"));
  const job = rendered.items.find(item => item.kind === "Job") as any;
  const pod = job.spec.template.spec;
  assert.equal(pod.automountServiceAccountToken, false);
  assert.equal(pod.hostNetwork, undefined);
  assert.deepEqual(pod.dnsConfig.nameservers, ["127.0.0.1"]);
  assert.equal(pod.initContainers[0].restartPolicy, "Always");
  assert.ok(!JSON.stringify(pod).includes("secretKeyRef"));
  assert.ok(!JSON.stringify(pod).includes("persistentVolumeClaim"));
  assert.throws(() => renderJob("mtc:latest", `node@sha256:${digest}`));
});

test("log verifier requires both exact failure classes, account correlation and zero dispatch", () => {
  const phases = [
    { phase: "healthy", status: 200, posts: 1 },
    { phase: "drop", account_id: "drop-account", status: 502, posts: 0, dns_queries: 1 },
    { phase: "nxdomain", account_id: "nx-account", status: 502, posts: 0, dns_queries: 1 },
  ].map(value => JSON.stringify(value)).join("\n");
  const logs = [
    { upstream_account_id: "drop-account", failure_category: "dns_timeout" },
    { upstream_account_id: "nx-account", failure_category: "dns_resolution" },
  ].map(fields => JSON.stringify({ fields: { ...fields, stage: "http_json_preparation", dispatched: false } })).join("\n");
  verifyLogs(logs, phases);
  assert.throws(() => verifyLogs(logs + CANARY, phases));
  assert.throws(() => verifyLogs(logs.replace("dns_timeout", "preparation_timeout"), phases));
  assert.throws(() => verifyLogs(logs, phases.replace('"dns_queries":1', '"dns_queries":0')));
  assert.throws(() => verifyLogs(logs.replace('"dispatched":false', '"dispatched":true'), phases));
});
