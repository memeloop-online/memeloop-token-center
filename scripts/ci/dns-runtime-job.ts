import assert from "node:assert/strict";
import { randomBytes } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { CANARY } from "./dns-runtime-probe.ts";
import { verifyCompactEvidence } from "./compact-runtime-probe.ts";

export function renderJob(image: string, nodeImage: string, suite: "dns" | "compact" = "dns") {
  assert.match(image, /^ghcr\.io\/memeloop-online\/memeloop-token-center(?::[^@]+)?@sha256:[a-f0-9]{64}$/);
  assert.match(nodeImage, /@sha256:[a-f0-9]{64}$/);
  const name = `mtc-${suite}-runtime-probe`;
  const namespace = "memeloop-token-center-cloud-test";
  const token = randomBytes(32).toString("hex");
  const labels = { "app.kubernetes.io/name": name };
  const mounts = [{ name: "data", mountPath: "/data" }];
  const security = {
    runAsNonRoot: true, runAsUser: 10001, runAsGroup: 10001,
    allowPrivilegeEscalation: false, readOnlyRootFilesystem: true,
    capabilities: { drop: ["ALL"] }, seccompProfile: { type: "RuntimeDefault" },
  };
  return {
    apiVersion: "v1", kind: "List", items: [
      {
        apiVersion: "v1", kind: "ConfigMap", metadata: { name, namespace },
        data: { "probe.ts": readFileSync(new URL(`./${suite}-runtime-probe.ts`, import.meta.url), "utf8") },
      },
      {
        apiVersion: "networking.k8s.io/v1", kind: "NetworkPolicy",
        metadata: { name, namespace },
        spec: { podSelector: { matchLabels: labels }, policyTypes: ["Ingress", "Egress"], ingress: [], egress: [] },
      },
      {
        apiVersion: "batch/v1", kind: "Job", metadata: { name, namespace },
        spec: {
          backoffLimit: 0, activeDeadlineSeconds: 240, ttlSecondsAfterFinished: 3600,
          template: {
            metadata: { labels },
            spec: {
              restartPolicy: "Never", automountServiceAccountToken: false,
              dnsPolicy: "None",
              dnsConfig: { nameservers: ["127.0.0.1"], options: [{ name: "attempts", value: "1" }, { name: "timeout", value: "4" }] },
              securityContext: { fsGroup: 10001 },
              // Native sidecar exits automatically when the probe completes.
              initContainers: [{
                name: "mtc", image, restartPolicy: "Always",
                args: ["serve", "--role", "all"], securityContext: security,
                resources: { requests: { cpu: "50m", memory: "128Mi" }, limits: { cpu: "1", memory: "512Mi" } },
                volumeMounts: mounts,
                env: Object.entries({
                  MTC_DATABASE_URL: "sqlite:///data/probe.sqlite?mode=rwc",
                  MTC_DATABASE_MAX_CONNECTIONS: "1", MTC_KEY_PEPPER: randomBytes(32).toString("hex"),
                  MTC_SERVICE_TOKEN: token, MTC_ALLOW_OAUTH_LOOPBACK: "true",
                  MTC_LISTEN: "127.0.0.1:8080", MTC_ARCHIVE_BACKEND: "filesystem",
                  MTC_ARCHIVE_PATH: "/data/archive", MTC_RUN_MIGRATIONS_ON_START: "true",
                  MTC_PROXY_MEMORY_BUDGET_BYTES: "268435456",
                }).map(([name, value]) => ({ name, value })),
              }],
              containers: [{
                name: "probe", image: nodeImage,
                command: ["node", "--experimental-strip-types", "/probe/probe.ts"],
                securityContext: suite === "dns"
                  ? { ...security, capabilities: { drop: ["ALL"], add: ["NET_BIND_SERVICE"] } }
                  : security,
                resources: { requests: { cpu: "25m", memory: "64Mi" }, limits: { cpu: "250m", memory: "128Mi" } },
                env: [{ name: "MTC_PROBE_SERVICE_TOKEN", value: token }],
                volumeMounts: [{ name: "script", mountPath: "/probe", readOnly: true }],
              }],
              volumes: [{ name: "data", emptyDir: {} }, { name: "script", configMap: { name } }],
            },
          },
        },
      },
    ],
  };
}

export function verifyLogs(mtcText: string, probeText: string) {
  assert.ok(!mtcText.includes(CANARY), "synthetic secret leaked into MTC log");
  assert.ok(!mtcText.includes("http://mtc-dns-probe.localhost"), "destination URL leaked");
  const phases = probeText.split(/\r?\n/).filter(Boolean).map(line => JSON.parse(line));
  assert.equal(phases.length, 4, "require exactly three phase results and terminal evidence");
  assert.deepEqual(phases.slice(0, 3).map(item => item.phase), ["healthy", "drop", "nxdomain"]);
  assert.deepEqual(phases[3], { transport_checks: "passed", log_verification: "required" });
  assert.equal(new Set(phases.slice(0, 3).map(item => item.account_id)).size, 3,
    "phases must use independent synthetic accounts");
  for (const phase of phases.slice(0, 3)) {
    assert.equal(typeof phase.account_id, "string");
    assert.ok(phase.account_id.length > 0);
    assert.ok(Number.isInteger(phase.dns_queries) && phase.dns_queries > 0,
      "each phase must observe DNS rather than an NSS shortcut");
  }
  const events = mtcText.split(/\r?\n/).filter(line => line.startsWith("{"))
    .map(line => JSON.parse(line));
  for (const [phase, category] of [["drop", "dns_timeout"], ["nxdomain", "dns_resolution"]]) {
    const fixture = phases.find(item => item.phase === phase);
    assert.ok(fixture && fixture.status >= 400 && fixture.dns_queries > 0 && fixture.posts === 0);
    assert.ok(events.some(event =>
      event.fields?.upstream_account_id === fixture.account_id &&
      event.fields?.stage === "http_json_preparation" &&
      event.fields?.failure_category === category && event.fields?.dispatched === false
    ), `missing ${category} diagnostic for synthetic account`);
  }
  assert.ok(phases.some(item => item.phase === "healthy" && item.status === 200 && item.posts === 1));
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const [action, first, second] = process.argv.slice(2);
  if (action === "verify-compact") {
    assert.ok(first && process.argv.length === 4, "usage: verify-compact PROBE_LOG");
    verifyCompactEvidence(readFileSync(first, "utf8"));
    console.log("synthetic compact transport verified");
  } else {
    assert.ok(first && second && process.argv.length === 5,
      "usage: render|render-compact MTC_IMAGE_DIGEST NODE22_IMAGE_DIGEST | verify-logs MTC_LOG PROBE_LOG");
    if (action === "render" || action === "render-compact") {
      console.log(JSON.stringify(renderJob(first, second, action === "render" ? "dns" : "compact"), null, 2));
    } else if (action === "verify-logs") {
      verifyLogs(readFileSync(first, "utf8"), readFileSync(second, "utf8"));
      console.log("synthetic DNS transport and diagnostics verified");
    } else {
      throw new Error("unknown runtime probe action");
    }
  }
}
