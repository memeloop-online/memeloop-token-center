import assert from "node:assert/strict";
import test from "node:test";
import { PHASES, accountConfig, inputForPhase, validateForwarded, validateMetadata, verifyUsageEvidence } from "./usage-runtime-probe.ts";
import { renderJob } from "./dns-runtime-job.ts";

test("account bound reserves per upstream model without imposing an undeclared generation cap", () => {
  assert.deepEqual(accountConfig().reservation_token_bounds, { "gpt-6-astra": 65536 });
  for (const phase of PHASES) {
    const body = { ...inputForPhase(phase), model: "gpt-6-astra" };
    const attempts = phase.kind === "insufficient_balance" ? [] : [{ path: "/v1/responses", body }];
    validateForwarded(phase, attempts);
    if (phase.kind === "uncapped") {
      assert.throws(() => validateForwarded(phase, [{
        path: "/v1/responses", body: { ...body, max_output_tokens: 65536 },
      }]));
    }
    if (phase.kind === "explicit_cap") {
      assert.throws(() => validateForwarded(phase, [{
        path: "/v1/responses", body: { ...body, max_output_tokens: 65536 },
      }]));
    }
    assert.throws(() => validateForwarded(phase,
      [...attempts, { path: "/v1/responses", body }]));
  }
});

test("settlement metadata catches streamed 200 that actually failed usage validation", () => {
  for (const phase of PHASES.filter(value => value.kind !== "insufficient_balance")) {
    const metadata = phase.kind === "uncapped"
      ? { status_code: 200, input_tokens: 3, output_tokens: 5000, cost: "0.005003" }
      : { status_code: 502, error_code: "upstream_invalid_usage", usage_basis: "not_observed", output_tokens: 0, cost: "0" };
    validateMetadata(phase, metadata);
    assert.throws(() => validateMetadata(phase, { ...metadata, output_tokens: 4096 }));
    assert.throws(() => validateMetadata(phase, { ...metadata, cost: "0.065539" }));
    if (phase.kind === "explicit_cap") {
      assert.throws(() => validateMetadata(phase, { ...metadata, status_code: 200 }));
    }
  }
});

test("runtime evidence requires all six phases and four total upstream POSTs", () => {
  const records = PHASES.map(phase => ({
    phase: phase.name,
    ...(phase.kind === "uncapped"
      ? { status: 200, input_tokens: 3, output_tokens: 5000, cost: 0.005003, posts: 1 }
      : phase.kind === "explicit_cap"
        ? { status: 502, output_tokens: 0, cost: 0, error_code: "upstream_invalid_usage", posts: 1 }
        : { status: 429, error_code: "balance_exhausted", posts: 0 }),
  }));
  const evidence = [...records, { usage_checks: "passed", phases: 6, posts: 4 }]
    .map(record => JSON.stringify(record)).join("\n");
  verifyUsageEvidence(evidence);
  assert.throws(() => verifyUsageEvidence(""));
  assert.throws(() => verifyUsageEvidence(evidence.replace('"posts":0', '"posts":1')));
  assert.throws(() => verifyUsageEvidence(evidence.split("\n").slice(0, -1).join("\n")));
});

test("usage Job has isolated name, synthetic credentials, ephemeral DB and writable Responses spool", () => {
  const digest = "a".repeat(64);
  const rendered = renderJob(`ghcr.io/memeloop-online/memeloop-token-center@sha256:${digest}`, `node:22@sha256:${digest}`, "usage");
  assert.ok(rendered.items.every(item => item.metadata.name === "mtc-usage-runtime-probe"));
  assert.ok(rendered.items.every(item => item.metadata.namespace === "memeloop-token-center-cloud-test"));
  const job = rendered.items.find(item => item.kind === "Job") as any;
  const pod = job.spec.template.spec;
  assert.equal(pod.automountServiceAccountToken, false);
  assert.ok(pod.initContainers[0].env.some((item: { name: string; value: string }) =>
    item.name === "MTC_RESPONSES_REQUEST_SPOOL_PATH" && item.value === "/data/request-spool"));
  assert.deepEqual(pod.containers[0].securityContext.capabilities, { drop: ["ALL"] });
  assert.ok(!JSON.stringify(pod).includes("secretKeyRef"));
  assert.ok(!JSON.stringify(pod).includes("persistentVolumeClaim"));
});
