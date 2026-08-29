import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { chmodSync, mkdtempSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";
import { RouteImportFailure, completeSinglePage, createPlan, execute, parseLiveRoute, parseManifest, parseSourceInventory, parseUpstreamInventory, readProtected, type LiveRoute, type RouteSpec, type RouteTarget } from "../../ops/legacy-routes/import-cpa-model-routes.ts";

const account = "018f1111-1111-7111-8111-111111111111";
const routeId = "018f2222-2222-7222-8222-222222222222";
const stable = "a".repeat(64);
const source = { provider: "codex-csil", model: "gpt-5.6-sol", group: null, upstream_prefix: "codex-csil", protocol: "openai" };
const json = (value: unknown): Buffer => Buffer.from(JSON.stringify(value));
const sourceDocument = (mappings: unknown[] = [source], reauthorization_required: unknown[] = [], anomalies: unknown[] = []): Buffer => json({ version: 1, mappings, reauthorization_required, anomalies });
const upstreamDocument = (): Buffer => json({ version: 1, tenant_external_id: "legacy", upstreams: [{ upstream_account_id: account, source_stable_id: stable, driver: "codex", status: "active", updated_at: 11 }] });
const manifestDocument = (sourceRaw: Buffer, upstreamRaw: Buffer, routes: unknown[]): Buffer => {
  const sha = (value: Buffer): string => createHash("sha256").update(value).digest("hex");
  return json({ version: 1, tenant_external_id: "legacy", target_api_base_url: "https://control.invalid/", source_inventory_sha256: sha(sourceRaw), upstream_inventory_sha256: sha(upstreamRaw), routes });
};
const routeSpec = (overrides: Record<string, unknown> = {}): Record<string, unknown> => ({ source, target: { upstream_account_id: account, source_stable_id: stable, public_model: "gpt-5.6-sol-csil", upstream_model: "gpt-5.6-sol", protocol: "openai", priority: 10 }, expected_existing: { action: "create", route_id: null, updated_at: null, grant_revision: null, history_and_references_reviewed: false, history_and_references_evidence_sha256: null }, ...overrides });
const live = (overrides: Partial<LiveRoute> = {}): LiveRoute => ({ id: routeId, publicModel: "gpt-5.6-sol-csil", upstreamModel: "gpt-5.6-sol", protocol: "openai", priority: 10, enabled: true, accountIds: [account], candidateAccountIds: [account], includedProviderGroupIds: [], excludedProviderGroupIds: [], routeGroupIds: [], grantedCredentialIds: [], customModelConfirmed: true, updatedAt: 22, grantRevision: 0, ...overrides });
const accountInventory = [{ id: account, driver: "codex", status: "active", updatedAt: 11 }];

class MemoryTarget implements RouteTarget {
  writes: string[] = [];
  readonly routes: LiveRoute[];
  constructor(routes: LiveRoute[] = []) { this.routes = routes; }
  async listRoutes(): Promise<LiveRoute[]> { return this.routes; }
  async listAccounts(): Promise<typeof accountInventory> { return accountInventory; }
  async create(_tenant: string, spec: RouteSpec): Promise<LiveRoute> { this.writes.push("create"); const result = live({ id: "018f3333-3333-7333-8333-333333333333", publicModel: spec.publicModel, upstreamModel: spec.upstreamModel, protocol: spec.protocol, priority: spec.priority, accountIds: [spec.accountId], candidateAccountIds: [spec.accountId] }); this.routes.push(result); return result; }
  async update(_tenant: string, spec: RouteSpec): Promise<LiveRoute> { this.writes.push("update"); return live({ publicModel: spec.publicModel, upstreamModel: spec.upstreamModel, protocol: spec.protocol, priority: spec.priority, accountIds: [spec.accountId], candidateAccountIds: [spec.accountId] }); }
}

function plan(routes: LiveRoute[], sourceRaw = sourceDocument(), manifestRoutes: unknown[] = [routeSpec()]) {
  const upstreamRaw = upstreamDocument(); return createPlan(sourceRaw, upstreamRaw, manifestDocument(sourceRaw, upstreamRaw, manifestRoutes), routes, accountInventory);
}

test("strict inventories reject unknown fields, duplicates, and loose JSON", () => {
  assert.throws(() => parseSourceInventory(json({ version: 1, mappings: [], reauthorization_required: [], anomalies: [], extra: true })), RouteImportFailure);
  assert.throws(() => parseSourceInventory(sourceDocument([source, source])), /duplicate/u);
  assert.throws(() => parseSourceInventory(Buffer.from('{"version":1,"mappings":[],"mappings":[],"reauthorization_required":[],"anomalies":[]}')), /strict/u);
  assert.throws(() => parseUpstreamInventory(json({ version: 1, tenant_external_id: "legacy", upstreams: [{ upstream_account_id: account, source_stable_id: stable, driver: "codex", status: "disabled", updated_at: 1 }] })), /inactive/u);
});

test("manifest pins both inventories and rejects provider merging and generation expansion", () => {
  const sourceRaw = sourceDocument(), upstreamRaw = upstreamDocument();
  assert.throws(() => parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec({ target: { upstream_account_id: account, source_stable_id: stable, public_model: "unsafe-image", upstream_model: "Qwen-Image", protocol: "generation", priority: 0 } })]), "0".repeat(64), "0".repeat(64)), /does not match/u);
  const duplicate = routeSpec({ source: { ...source, provider: "codex-second" } });
  assert.throws(() => parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec(), duplicate]), hash(sourceRaw), hash(upstreamRaw)), /merges provider-specific/u);
  assert.throws(() => parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec({ target: { upstream_account_id: account, source_stable_id: stable, public_model: "unsafe-image", upstream_model: "Qwen-Image", protocol: "generation", priority: 0 } })]), hash(sourceRaw), hash(upstreamRaw)), /schema/u);
  assert.throws(() => parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec({ target: { upstream_account_id: account, source_stable_id: stable, public_model: "gpt-5.6-sol-csil", upstream_model: "gpt-5.6-sol", protocol: "anthropic", priority: 10 } })]), hash(sourceRaw), hash(upstreamRaw)), /protocol differs/u);
  assert.throws(() => parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec({ target: { upstream_account_id: account, source_stable_id: stable, public_model: "gpt-5.6-sol-csil", upstream_model: "gpt-5.6-sol", protocol: "openai", priority: 1_000_001 } })]), hash(sourceRaw), hash(upstreamRaw)), /schema/u);
});

test("provider-specific same public model requires owner-selected distinct priorities", () => {
  const second = { ...source, provider: "codex-dongwu", upstream_prefix: "codex-dongwu" };
  const sourceRaw = sourceDocument([source, second]), upstreamRaw = upstreamDocument();
  const collision = routeSpec({ source: second });
  assert.throws(() => parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec(), collision]), hash(sourceRaw), hash(upstreamRaw)), /merges provider-specific/u);
  const distinct = routeSpec({ source: second, target: { upstream_account_id: account, source_stable_id: stable, public_model: "gpt-5.6-sol-csil", upstream_model: "gpt-5.6-sol", protocol: "openai", priority: 20 } });
  assert.equal(parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec(), distinct]), hash(sourceRaw), hash(upstreamRaw)).specs.length, 2);
});

test("dry-run plans exact create and apply verifies topology with count-only checkpoint", async () => {
  const selected = plan([]), target = new MemoryTarget(), directory = mkdtempSync(join(tmpdir(), "mtc-routes-")), checkpoint = join(directory, "checkpoint.json");
  const dry = await execute(selected, "legacy", target, false, checkpoint); assert.equal(dry.create_count, 1); assert.equal(target.writes.length, 0); assert.equal(statSync(checkpoint).mode & 0o777, 0o600);
  const result = await execute(selected, "legacy", target, true, checkpoint); assert.deepEqual(target.writes, ["create"]); assert.equal(result.verified_count, 1); assert.equal(result.written_count, 1);
  const receipt = JSON.parse(readFileSync(checkpoint, "utf8")); assert.deepEqual(Object.keys(receipt).sort(), ["completed_count", "failed_count", "mode", "plan_sha256", "planned_count", "reviewed_manifest_sha256", "source_inventory_sha256", "upstream_inventory_sha256", "version"]); assert.equal(receipt.completed_count, 1);
});

test("identical route replay performs zero writes", async () => { const selected = plan([live()]), target = new MemoryTarget([live()]), checkpoint = join(mkdtempSync(join(tmpdir(), "mtc-route-replay-")), "checkpoint.json"); const result = await execute(selected, "legacy", target, true, checkpoint); assert.equal(selected.counts.replay_count, 1); assert.equal(result.written_count, 0); assert.equal(target.writes.length, 0); });

test("CAS update requires exact reviewed id, revisions, and history/reference acknowledgement", async () => {
  const expected = { action: "update", route_id: routeId, updated_at: 22, grant_revision: 0, history_and_references_reviewed: true, history_and_references_evidence_sha256: "b".repeat(64) };
  const selected = plan([live({ upstreamModel: "old" })], sourceDocument(), [routeSpec({ expected_existing: expected })]); assert.equal(selected.counts.update_count, 1);
  const target = new MemoryTarget([live({ upstreamModel: "old" })]); const result = await execute(selected, "legacy", target, true, join(mkdtempSync(join(tmpdir(), "mtc-route-update-")), "checkpoint.json")); assert.deepEqual(target.writes, ["update"]); assert.equal(result.written_count, 1);
  assert.equal(plan([live({ upstreamModel: "old", updatedAt: 23 })], sourceDocument(), [routeSpec({ expected_existing: expected })]).counts.conflict_count, 1);
  assert.throws(() => { const sourceRaw = sourceDocument(), upstreamRaw = upstreamDocument(); parseManifest(manifestDocument(sourceRaw, upstreamRaw, [routeSpec({ expected_existing: { ...expected, history_and_references_reviewed: false } })]), hash(sourceRaw), hash(upstreamRaw)); }, /incomplete/u);
});

test("provider expansion, route-group grants, and stale upstream inventory fail closed", () => {
  assert.equal(plan([live({ accountIds: [account, "018f4444-4444-7444-8444-444444444444"], candidateAccountIds: [account, "018f4444-4444-7444-8444-444444444444"] })]).counts.conflict_count, 1);
  assert.equal(plan([live({ routeGroupIds: ["018f5555-5555-7555-8555-555555555555"] })]).counts.conflict_count, 1);
  const sourceRaw = sourceDocument(), upstreamRaw = upstreamDocument(), manifestRaw = manifestDocument(sourceRaw, upstreamRaw, [routeSpec()]);
  assert.throws(() => createPlan(sourceRaw, upstreamRaw, manifestRaw, [], [{ ...accountInventory[0]!, updatedAt: 12 }]), /stale or conflicting/u);
});

test("update never clears existing grants or provider groups", () => {
  const expected = { action: "update", route_id: routeId, updated_at: 22, grant_revision: 0, history_and_references_reviewed: true, history_and_references_evidence_sha256: "b".repeat(64) };
  for (const unsafe of [
    live({ upstreamModel: "old", grantedCredentialIds: ["018f5555-5555-7555-8555-555555555555"] }),
    live({ upstreamModel: "old", routeGroupIds: ["018f5555-5555-7555-8555-555555555555"] }),
    live({ upstreamModel: "old", includedProviderGroupIds: ["018f5555-5555-7555-8555-555555555555"] }),
  ]) assert.equal(plan([unsafe], sourceDocument(), [routeSpec({ expected_existing: expected })]).counts.conflict_count, 1);
});

test("apply requires checkpoint and reconciles a lost create response as exact replay", async () => {
  const initial = plan([]), target = new MemoryTarget(), directory = mkdtempSync(join(tmpdir(), "mtc-route-lost-")), checkpoint = join(directory, "checkpoint.json");
  await assert.rejects(execute(initial, "legacy", target, true), /requires.*checkpoint/u);
  let first = true;
  target.create = async (_tenant, spec) => {
    if (!first) return target.routes[0]!;
    first = false; const created = live({ id: "018f3333-3333-7333-8333-333333333333", publicModel: spec.publicModel, upstreamModel: spec.upstreamModel, protocol: spec.protocol, priority: spec.priority, accountIds: [spec.accountId], candidateAccountIds: [spec.accountId] }); target.routes.push(created); throw new Error("simulated lost response");
  };
  await assert.rejects(execute(initial, "legacy", target, true, checkpoint), /mutation failed/u);
  assert.equal(JSON.parse(readFileSync(checkpoint, "utf8")).completed_count, 0);
  const reconciled = plan(target.routes); assert.equal(reconciled.planDigest, initial.planDigest); assert.equal(reconciled.counts.replay_count, 1);
  const result = await execute(reconciled, "legacy", target, true, checkpoint); assert.equal(result.written_count, 0); assert.equal(JSON.parse(readFileSync(checkpoint, "utf8")).completed_count, 1);
});

test("resume revalidates completed routes and dry-run cannot erase apply progress", async () => {
  const target = new MemoryTarget(), directory = mkdtempSync(join(tmpdir(), "mtc-route-resume-")), checkpoint = join(directory, "checkpoint.json"), initial = plan([]);
  await execute(initial, "legacy", target, true, checkpoint);
  const replay = plan(target.routes); await assert.rejects(execute(replay, "legacy", target, false, checkpoint), /cannot overwrite/u);
  const drifted = plan([]); await assert.rejects(execute(drifted, "legacy", new MemoryTarget(), true, checkpoint), /no longer exact/u);
});

test("single-page live inventory boundary fails closed instead of truncating", () => {
  assert.equal(completeSinglePage(Array.from({ length: 99 }), "target route").length, 99);
  assert.throws(() => completeSinglePage(Array.from({ length: 100 }), "target route"), /safety boundary/u);
});

test("live inventory accepts unrelated generation routes but reviewed legacy targets do not", () => {
  const value = { id: routeId, public_model: "qwen-image", upstream_model: "Qwen/Qwen-Image", protocol: "generation", priority: 0, enabled: true, upstream_account_ids: [account], candidate_upstream_account_ids: [account], included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [], granted_credential_ids: [], custom_model_confirmed: true, updated_at: 1, grant_revision: 0 };
  assert.equal(parseLiveRoute(value).protocol, "generation");
});

test("missing mappings and the preserved alias anomaly block apply while reauthorization remains report-only", async () => {
  const second = { ...source, provider: "deepseek-self", model: "deepseek-v4-pro" };
  const missing = plan([], sourceDocument([source, second])); assert.equal(missing.counts.unmatched_mapping_count, 1); await assert.rejects(execute(missing, "legacy", new MemoryTarget(), true), /blocked/u);
  const anomalySource = sourceDocument([source], [{ provider: "copilot", model: "gpt-5-mini", reason: "native OAuth must be reauthorized" }], [{ provider: "codex", model: "classify:csil", reason: "legacy field-shape anomaly; do not infer group/model" }]);
  const anomaly = plan([], anomalySource); assert.equal(anomaly.counts.reauthorization_required_count, 1); assert.equal(anomaly.counts.anomaly_count, 1); await assert.rejects(execute(anomaly, "legacy", new MemoryTarget(), true), /blocked/u);
});

test("partial failure persists count-only checkpoint without identifiers or secrets", async () => {
  const sourceRaw = sourceDocument(), selected = plan([]), directory = mkdtempSync(join(tmpdir(), "mtc-routes-fail-")), path = join(directory, "checkpoint.json");
  const target = new MemoryTarget(); target.create = async () => { throw new Error(`must redact ${account} secret-value account@example.test`); };
  await assert.rejects(execute(selected, "legacy", target, true, path), /mutation failed/u);
  const output = readFileSync(path, "utf8"); assert.doesNotMatch(output, /018f|secret|example\.test|codex-csil|gpt-5/u); assert.equal(JSON.parse(output).failed_count, 1); assert.ok(hash(sourceRaw));
});

test("protected input files require owner-only regular files", () => { const directory = mkdtempSync(join(tmpdir(), "mtc-route-input-")), path = join(directory, "input.json"); writeFileSync(path, "{}", { mode: 0o600 }); assert.equal(readProtected(path, "fixture").toString(), "{}"); chmodSync(path, 0o640); assert.throws(() => readProtected(path, "fixture"), /0600/u); });

function hash(value: Buffer): string { return createHash("sha256").update(value).digest("hex"); }
