#!/usr/bin/env node
/** Owner-reviewed, provider-exact CPA model-route convergence. Dry-run is the default. */

import { createHash } from "node:crypto";
import { constants, closeSync, existsSync, fstatSync, openSync, readSync, renameSync, statSync, unlinkSync, writeFileSync } from "node:fs";
import { request as httpRequest } from "node:http";
import { request as httpsRequest } from "node:https";
import { parseStrictJson } from "../lib/strict-json.ts";

type Obj = Record<string, unknown>;
type Protocol = "openai" | "anthropic";
type LiveProtocol = Protocol | "generation";
type SourcePattern = Readonly<{ provider: string; model: string; group: string | null; upstreamPrefix: string | null; protocol: Protocol }>;
type SourceInventory = Readonly<{ mappings: readonly SourcePattern[]; reauthorizationRequired: number; anomalies: number }>;
type Upstream = Readonly<{ accountId: string; sourceStableId: string; driver: string; status: "active"; updatedAt: number }>;
type ExistingExpectation = Readonly<{ action: "create" | "update"; routeId: string | null; updatedAt: number | null; grantRevision: number | null; historyAndReferencesReviewed: boolean; historyAndReferencesEvidenceDigest: string | null }>;
export type RouteSpec = Readonly<{ source: SourcePattern; accountId: string; sourceStableId: string; publicModel: string; upstreamModel: string; protocol: Protocol; priority: number; existing: ExistingExpectation }>;
export type LiveRoute = Readonly<{ id: string; publicModel: string; upstreamModel: string; protocol: LiveProtocol; priority: number; enabled: boolean; accountIds: readonly string[]; candidateAccountIds: readonly string[]; includedProviderGroupIds: readonly string[]; excludedProviderGroupIds: readonly string[]; routeGroupIds: readonly string[]; grantedCredentialIds: readonly string[]; customModelConfirmed: boolean; updatedAt: number; grantRevision: number }>;
type PlanItem = Readonly<{ spec: RouteSpec; outcome: "create" | "replay" | "update" | "conflict"; live?: LiveRoute }>;
export type RoutePlan = Readonly<{ items: readonly PlanItem[]; sourceDigest: string; upstreamDigest: string; manifestDigest: string; planDigest: string; targetBaseUrl: string; counts: Readonly<Record<string, number>> }>;

const UUID = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/;
const SHA = /^[0-9a-f]{64}$/;
const TOKEN = /^[^\0\r\n]{1,16384}$/;
const MAX_BYTES = 8 * 1024 * 1024;

export class RouteImportFailure extends Error {
  readonly counts?: Readonly<Record<string, number>>;
  constructor(message: string, counts?: Readonly<Record<string, number>>) { super(message); this.counts = counts; }
}
const digest = (value: Uint8Array | string): string => createHash("sha256").update(value).digest("hex");
const encode = (value: unknown): string => JSON.stringify(value);
function record(value: unknown, keys: readonly string[], label: string): Obj {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RouteImportFailure(`${label} has an invalid schema`);
  const actual = Object.keys(value as Obj).sort(), expected = [...keys].sort();
  if (actual.length !== expected.length || actual.some((key, index) => key !== expected[index])) throw new RouteImportFailure(`${label} has an invalid schema`);
  return value as Obj;
}
function text(value: unknown, label: string, pattern?: RegExp): string {
  if (typeof value !== "string" || value.trim() !== value || value.length === 0 || Buffer.byteLength(value) > 500 || /[\0\r\n]/.test(value) || (pattern && !pattern.test(value))) throw new RouteImportFailure(`${label} has an invalid schema`);
  return value;
}
function optionalText(value: unknown, label: string): string | null { return value === null ? null : text(value, label); }
function integer(value: unknown, label: string, min = 0, max = 1_000_000_000_000_000): number { if (!Number.isSafeInteger(value) || Number(value) < min || Number(value) > max) throw new RouteImportFailure(`${label} has an invalid schema`); return Number(value); }
function protocol(value: unknown, label: string): Protocol { if (value !== "openai" && value !== "anthropic") throw new RouteImportFailure(`${label} has an invalid schema`); return value; }
function liveProtocol(value: unknown, label: string): LiveProtocol { if (value !== "openai" && value !== "anthropic" && value !== "generation") throw new RouteImportFailure(`${label} has an invalid schema`); return value; }
function strictJson(raw: Buffer, label: string): unknown { try { return parseStrictJson(new TextDecoder("utf-8", { fatal: true }).decode(raw)); } catch { throw new RouteImportFailure(`${label} is not strict UTF-8 JSON`); } }
function stringArray(value: unknown, label: string): string[] {
  if (!Array.isArray(value) || value.length > 500) throw new RouteImportFailure(`${label} has an invalid schema`);
  const output = value.map((item) => text(item, label, UUID)).sort();
  if (new Set(output).size !== output.length) throw new RouteImportFailure(`${label} contains duplicates`);
  return output;
}
function same(left: readonly unknown[], right: readonly unknown[]): boolean { return left.length === right.length && left.every((item, index) => item === right[index]); }
function sourcePattern(value: unknown, label: string): SourcePattern {
  const item = record(value, ["provider", "model", "group", "upstream_prefix", "protocol"], label);
  return { provider: text(item.provider, label), model: text(item.model, label), group: optionalText(item.group, label), upstreamPrefix: optionalText(item.upstream_prefix, label), protocol: protocol(item.protocol, label) };
}
function sourceKey(value: SourcePattern): string { return encode([value.provider, value.model, value.group, value.upstreamPrefix, value.protocol]); }

export function readProtected(path: string, label: string): Buffer {
  let fd: number | undefined;
  try {
    fd = openSync(path, constants.O_RDONLY | constants.O_NOFOLLOW); const stat = fstatSync(fd);
    if (!stat.isFile() || stat.nlink !== 1 || (stat.mode & 0o777) !== 0o600) throw new RouteImportFailure(`${label} must be a mode 0600 regular file with one link`);
    const buffer = Buffer.allocUnsafe(MAX_BYTES + 1); let offset = 0;
    while (offset < buffer.length) { const count = readSync(fd, buffer, offset, buffer.length - offset, null); if (!count) break; offset += count; }
    if (offset > MAX_BYTES) { buffer.fill(0); throw new RouteImportFailure(`${label} exceeds the size limit`); }
    return Buffer.from(buffer.subarray(0, offset));
  } catch (error) { if (error instanceof RouteImportFailure) throw error; throw new RouteImportFailure(`${label} is not safely readable`); }
  finally { if (fd !== undefined) closeSync(fd); }
}

export function parseSourceInventory(raw: Buffer): SourceInventory {
  const root = record(strictJson(raw, "source inventory"), ["version", "mappings", "reauthorization_required", "anomalies"], "source inventory");
  if (root.version !== 1 || !Array.isArray(root.mappings) || !Array.isArray(root.reauthorization_required) || !Array.isArray(root.anomalies) || root.mappings.length > 1000) throw new RouteImportFailure("source inventory has an invalid schema");
  const mappings = root.mappings.map((item) => sourcePattern(item, "source mapping"));
  if (new Set(mappings.map(sourceKey)).size !== mappings.length) throw new RouteImportFailure("source inventory contains duplicate mappings");
  for (const [field, values] of [["reauthorization", root.reauthorization_required], ["anomaly", root.anomalies]] as const) {
    for (const value of values) {
      const item = record(value, ["provider", "model", "reason"], `source ${field}`);
      text(item.provider, `source ${field}`); text(item.model, `source ${field}`); text(item.reason, `source ${field}`);
    }
  }
  return { mappings, reauthorizationRequired: root.reauthorization_required.length, anomalies: root.anomalies.length };
}

export function parseUpstreamInventory(raw: Buffer): { tenant: string; upstreams: Upstream[] } {
  const root = record(strictJson(raw, "upstream inventory"), ["version", "tenant_external_id", "upstreams"], "upstream inventory");
  if (root.version !== 1 || !Array.isArray(root.upstreams) || root.upstreams.length > 1000) throw new RouteImportFailure("upstream inventory has an invalid schema");
  const tenant = text(root.tenant_external_id, "upstream inventory tenant");
  const upstreams = root.upstreams.map((value) => { const item = record(value, ["upstream_account_id", "source_stable_id", "driver", "status", "updated_at"], "upstream inventory item"); if (item.status !== "active") throw new RouteImportFailure("upstream inventory selects an inactive account"); return { accountId: text(item.upstream_account_id, "upstream inventory item", UUID), sourceStableId: text(item.source_stable_id, "upstream inventory item", SHA), driver: text(item.driver, "upstream inventory item"), status: "active" as const, updatedAt: integer(item.updated_at, "upstream inventory item") }; });
  if (new Set(upstreams.map((item) => item.accountId)).size !== upstreams.length || new Set(upstreams.map((item) => item.sourceStableId)).size !== upstreams.length) throw new RouteImportFailure("upstream inventory contains duplicate source bindings");
  return { tenant, upstreams };
}

export function parseManifest(raw: Buffer, sourceDigest: string, upstreamDigest: string): { tenant: string; targetBaseUrl: string; specs: RouteSpec[] } {
  const root = record(strictJson(raw, "reviewed manifest"), ["version", "tenant_external_id", "target_api_base_url", "source_inventory_sha256", "upstream_inventory_sha256", "routes"], "reviewed manifest");
  if (root.version !== 1 || text(root.source_inventory_sha256, "reviewed manifest", SHA) !== sourceDigest || text(root.upstream_inventory_sha256, "reviewed manifest", SHA) !== upstreamDigest || !Array.isArray(root.routes) || root.routes.length > 1000) throw new RouteImportFailure("reviewed manifest does not match the selected inventories");
  const tenant = text(root.tenant_external_id, "reviewed manifest tenant");
  const targetBaseUrl = text(root.target_api_base_url, "reviewed target API base URL");
  const specs = root.routes.map((value) => {
    const item = record(value, ["source", "target", "expected_existing"], "reviewed route");
    const target = record(item.target, ["upstream_account_id", "source_stable_id", "public_model", "upstream_model", "protocol", "priority"], "reviewed target");
    const expected = record(item.expected_existing, ["action", "route_id", "updated_at", "grant_revision", "history_and_references_reviewed", "history_and_references_evidence_sha256"], "reviewed existing route");
    if (expected.action !== "create" && expected.action !== "update") throw new RouteImportFailure("reviewed existing route has an invalid schema");
    if (typeof expected.history_and_references_reviewed !== "boolean") throw new RouteImportFailure("reviewed existing route has an invalid schema");
    const routeId = expected.route_id === null ? null : text(expected.route_id, "reviewed existing route", UUID);
    const updatedAt = expected.updated_at === null ? null : integer(expected.updated_at, "reviewed existing route");
    const grantRevision = expected.grant_revision === null ? null : integer(expected.grant_revision, "reviewed existing route");
    const evidenceDigest = expected.history_and_references_evidence_sha256 === null ? null : text(expected.history_and_references_evidence_sha256, "reviewed existing route", SHA);
    if (expected.action === "create" ? routeId !== null || updatedAt !== null || grantRevision !== null || expected.history_and_references_reviewed || evidenceDigest !== null : routeId === null || updatedAt === null || grantRevision === null || !expected.history_and_references_reviewed || evidenceDigest === null) throw new RouteImportFailure("reviewed existing route action is incomplete");
    const action: "create" | "update" = expected.action;
    const targetProtocol = protocol(target.protocol, "reviewed target"); if (targetProtocol !== sourcePattern(item.source, "reviewed source").protocol) throw new RouteImportFailure("reviewed target protocol differs from its exact source mapping");
    return { source: sourcePattern(item.source, "reviewed source"), accountId: text(target.upstream_account_id, "reviewed target", UUID), sourceStableId: text(target.source_stable_id, "reviewed target", SHA), publicModel: text(target.public_model, "reviewed target"), upstreamModel: text(target.upstream_model, "reviewed target"), protocol: targetProtocol, priority: integer(target.priority, "reviewed target", -1_000_000, 1_000_000), existing: { action, routeId, updatedAt, grantRevision, historyAndReferencesReviewed: expected.history_and_references_reviewed, historyAndReferencesEvidenceDigest: evidenceDigest } };
  });
  if (new Set(specs.map((item) => sourceKey(item.source))).size !== specs.length) throw new RouteImportFailure("reviewed manifest maps a source more than once");
  if (new Set(specs.map((item) => encode([item.publicModel, item.protocol, item.priority]))).size !== specs.length) throw new RouteImportFailure("reviewed manifest merges provider-specific routes");
  return { tenant, targetBaseUrl, specs };
}

function routeMatches(route: LiveRoute, spec: RouteSpec): boolean {
  return route.enabled && route.publicModel === spec.publicModel && route.upstreamModel === spec.upstreamModel && route.protocol === spec.protocol && route.priority === spec.priority && same(route.accountIds, [spec.accountId]) && same(route.candidateAccountIds, [spec.accountId]) && route.includedProviderGroupIds.length === 0 && route.excludedProviderGroupIds.length === 0 && route.routeGroupIds.length === 0 && route.grantedCredentialIds.length === 0 && route.customModelConfirmed;
}
function summary(source: SourceInventory, items: readonly PlanItem[]): Record<string, number> {
  return { source_mapping_count: source.mappings.length, matched_mapping_count: items.length, unmatched_mapping_count: source.mappings.length - items.length, create_count: items.filter((item) => item.outcome === "create").length, replay_count: items.filter((item) => item.outcome === "replay").length, update_count: items.filter((item) => item.outcome === "update").length, conflict_count: items.filter((item) => item.outcome === "conflict").length, reauthorization_required_count: source.reauthorizationRequired, anomaly_count: source.anomalies };
}

export function createPlan(sourceRaw: Buffer, upstreamRaw: Buffer, manifestRaw: Buffer, liveRoutes: readonly LiveRoute[], liveAccounts: readonly { id: string; driver: string; status: string; updatedAt: number }[]): RoutePlan {
  const sourceDigest = digest(sourceRaw), upstreamDigest = digest(upstreamRaw), manifestDigest = digest(manifestRaw);
  const source = parseSourceInventory(sourceRaw), inventory = parseUpstreamInventory(upstreamRaw), manifest = parseManifest(manifestRaw, sourceDigest, upstreamDigest);
  if (inventory.tenant !== manifest.tenant) throw new RouteImportFailure("inventory tenant and reviewed tenant differ");
  const upstreams = new Map(inventory.upstreams.map((item) => [item.accountId, item]));
  const accounts = new Map(liveAccounts.map((item) => [item.id, item]));
  for (const spec of manifest.specs) { const upstream = upstreams.get(spec.accountId), account = accounts.get(spec.accountId); if (!upstream || upstream.sourceStableId !== spec.sourceStableId || !account || account.driver !== upstream.driver || account.status !== upstream.status || account.updatedAt !== upstream.updatedAt) throw new RouteImportFailure("reviewed target upstream binding is stale or conflicting"); }
  const sourceSet = new Set(source.mappings.map(sourceKey)), manifestSet = new Set(manifest.specs.map((item) => sourceKey(item.source)));
  if ([...manifestSet].some((key) => !sourceSet.has(key))) throw new RouteImportFailure("reviewed manifest contains a source absent from inventory");
  const items: PlanItem[] = [];
  for (const spec of manifest.specs) {
    const collisions = liveRoutes.filter((route) => route.publicModel === spec.publicModel && route.protocol === spec.protocol && route.priority === spec.priority);
    const expected = spec.existing.routeId ? liveRoutes.find((route) => route.id === spec.existing.routeId) : undefined;
    if (spec.existing.action === "update") {
      if (!expected || expected.updatedAt !== spec.existing.updatedAt || expected.grantRevision !== spec.existing.grantRevision || !collisions.every((route) => route.id === expected.id)) items.push({ spec, outcome: "conflict", live: expected ?? collisions[0] });
      else if (routeMatches(expected, spec)) items.push({ spec, outcome: "replay", live: expected });
      else if (expected.grantedCredentialIds.length || expected.routeGroupIds.length || expected.includedProviderGroupIds.length || expected.excludedProviderGroupIds.length) items.push({ spec, outcome: "conflict", live: expected });
      else items.push({ spec, outcome: "update", live: expected });
    } else if (collisions.length === 1 && routeMatches(collisions[0]!, spec)) items.push({ spec, outcome: "replay", live: collisions[0] });
    else if (collisions.length === 0) items.push({ spec, outcome: "create" });
    else items.push({ spec, outcome: "conflict", live: collisions[0] });
  }
  const counts = summary(source, items);
  // The intent digest deliberately excludes live outcomes. After an acknowledged
  // create whose response was lost, the same reviewed intent changes from
  // `create` to `replay` without invalidating the resume checkpoint.
  const planDigest = digest(encode({ sourceDigest, upstreamDigest, manifestDigest, itemCount: items.length }));
  return { items, sourceDigest, upstreamDigest, manifestDigest, planDigest, targetBaseUrl: manifest.targetBaseUrl, counts };
}

export interface RouteTarget { listRoutes(tenant: string): Promise<LiveRoute[]>; listAccounts(tenant: string): Promise<{ id: string; driver: string; status: string; updatedAt: number }[]>; create(tenant: string, spec: RouteSpec): Promise<LiveRoute>; update(tenant: string, spec: RouteSpec): Promise<LiveRoute>; }
function checkpoint(path: string, plan: RoutePlan, mode: "dry-run" | "apply", completed: number, failed: number): void {
  const payload = `${encode({ version: 1, mode, source_inventory_sha256: plan.sourceDigest, upstream_inventory_sha256: plan.upstreamDigest, reviewed_manifest_sha256: plan.manifestDigest, plan_sha256: plan.planDigest, planned_count: plan.items.length, completed_count: completed, failed_count: failed })}\n`;
  const temporary = `${path}.tmp`; try { unlinkSync(temporary); } catch {}
  writeFileSync(temporary, payload, { mode: 0o600, flag: "wx" }); renameSync(temporary, path);
  const metadata = statSync(path); if (!metadata.isFile() || metadata.nlink !== 1 || (metadata.mode & 0o777) !== 0o600) throw new RouteImportFailure("checkpoint could not be persisted safely");
}
function resumeState(path: string, plan: RoutePlan): { completed: number; mode: "dry-run" | "apply" } {
  if (!existsSync(path)) return { completed: 0, mode: "dry-run" };
  const root = record(strictJson(readProtected(path, "checkpoint"), "checkpoint"), ["version", "mode", "source_inventory_sha256", "upstream_inventory_sha256", "reviewed_manifest_sha256", "plan_sha256", "planned_count", "completed_count", "failed_count"], "checkpoint");
  if (root.version !== 1 || (root.mode !== "dry-run" && root.mode !== "apply") || root.source_inventory_sha256 !== plan.sourceDigest || root.upstream_inventory_sha256 !== plan.upstreamDigest || root.reviewed_manifest_sha256 !== plan.manifestDigest || root.plan_sha256 !== plan.planDigest || root.planned_count !== plan.items.length) throw new RouteImportFailure("checkpoint does not match the selected route plan");
  const completed = integer(root.completed_count, "checkpoint"), failed = integer(root.failed_count, "checkpoint", 0, 1); if (completed > plan.items.length || failed > 1 || (root.mode === "dry-run" && completed !== 0)) throw new RouteImportFailure("checkpoint has an invalid schema"); return { completed, mode: root.mode };
}
export async function execute(plan: RoutePlan, tenant: string, target: RouteTarget, apply: boolean, checkpointPath?: string): Promise<Record<string, number>> {
  const counts: Record<string, number> = { ...plan.counts, written_count: 0, verified_count: 0, failed_count: 0 };
  if (counts.unmatched_mapping_count || counts.conflict_count || counts.anomaly_count) throw new RouteImportFailure("route convergence is blocked by unmatched or conflicting source mappings", counts);
  if (!apply) { if (checkpointPath) { const previous = resumeState(checkpointPath, plan); if (previous.mode === "apply") throw new RouteImportFailure("dry-run cannot overwrite an apply checkpoint"); checkpoint(checkpointPath, plan, "dry-run", 0, 0); } return counts; }
  if (!checkpointPath) throw new RouteImportFailure("apply requires an owner-only checkpoint file", counts);
  let completed = resumeState(checkpointPath, plan).completed;
  if (plan.items.slice(0, completed).some((item) => item.outcome !== "replay" || !item.live || !routeMatches(item.live, item.spec))) throw new RouteImportFailure("checkpointed target routes are no longer exact replays", counts);
  counts.resumed_count = completed; counts.verified_count = completed;
  try {
    for (const item of plan.items.slice(completed)) {
      if (item.outcome === "replay") { completed++; counts.verified_count = (counts.verified_count ?? 0) + 1; continue; }
      const result = item.outcome === "create" ? await target.create(tenant, item.spec) : await target.update(tenant, item.spec);
      counts.written_count = (counts.written_count ?? 0) + 1; if (!routeMatches(result, item.spec)) throw new RouteImportFailure("target route topology verification failed");
      completed++; counts.verified_count = (counts.verified_count ?? 0) + 1;
      checkpoint(checkpointPath, plan, "apply", completed, 0);
    }
  } catch (error) { counts.failed_count = 1; checkpoint(checkpointPath, plan, "apply", completed, 1); if (error instanceof RouteImportFailure) throw new RouteImportFailure(error.message, counts); throw new RouteImportFailure("target route mutation failed", counts); }
  checkpoint(checkpointPath, plan, "apply", completed, 0);
  return counts;
}

export function parseLiveRoute(value: unknown): LiveRoute {
  if (!value || typeof value !== "object" || Array.isArray(value)) throw new RouteImportFailure("target route response is invalid"); const item = value as Obj;
  for (const key of ["id", "public_model", "upstream_model", "protocol", "priority", "enabled", "upstream_account_ids", "candidate_upstream_account_ids", "included_provider_group_ids", "excluded_provider_group_ids", "route_group_ids", "granted_credential_ids", "custom_model_confirmed", "updated_at", "grant_revision"]) if (!Object.hasOwn(item, key)) throw new RouteImportFailure("target route response is invalid");
  if (typeof item.enabled !== "boolean" || typeof item.custom_model_confirmed !== "boolean") throw new RouteImportFailure("target route response is invalid");
  return { id: text(item.id, "target route", UUID), publicModel: text(item.public_model, "target route"), upstreamModel: text(item.upstream_model, "target route"), protocol: liveProtocol(item.protocol, "target route"), priority: integer(item.priority, "target route", -1_000_000, 1_000_000), enabled: item.enabled, accountIds: stringArray(item.upstream_account_ids, "target route"), candidateAccountIds: stringArray(item.candidate_upstream_account_ids, "target route"), includedProviderGroupIds: stringArray(item.included_provider_group_ids, "target route"), excludedProviderGroupIds: stringArray(item.excluded_provider_group_ids, "target route"), routeGroupIds: stringArray(item.route_group_ids, "target route"), grantedCredentialIds: stringArray(item.granted_credential_ids, "target route"), customModelConfirmed: item.custom_model_confirmed, updatedAt: integer(item.updated_at, "target route"), grantRevision: integer(item.grant_revision, "target route") };
}

class HttpTarget implements RouteTarget {
  private readonly base: URL;
  private readonly token: Buffer;
  constructor(base: URL, token: Buffer) { this.base = base; this.token = token; }
  private async request(method: string, path: string, body?: unknown): Promise<unknown> {
    const url = new URL(path, this.base); const payload = body === undefined ? undefined : Buffer.from(encode(body));
    return await new Promise((resolve, reject) => { const transport = url.protocol === "https:" ? httpsRequest : httpRequest; const request = transport(url, { method, headers: { authorization: `Bearer ${this.token.toString("utf8")}`, accept: "application/json", ...(payload ? { "content-type": "application/json", "content-length": String(payload.length) } : {}) } }, (response) => { const parts: Buffer[] = []; let bytes = 0; response.on("data", (part: Buffer) => { bytes += part.length; if (bytes > MAX_BYTES) request.destroy(new RouteImportFailure("target response exceeds the size limit")); else parts.push(part); }); response.on("end", () => { const raw = Buffer.concat(parts); if (!response.statusCode || response.statusCode < 200 || response.statusCode > 299) { raw.fill(0); reject(new RouteImportFailure("target API request failed")); return; } try { resolve(strictJson(raw, "target response")); } catch (error) { reject(error); } finally { raw.fill(0); } }); }); request.on("error", () => reject(new RouteImportFailure("target API request failed"))); request.setTimeout(30_000, () => request.destroy()); if (payload) request.end(payload); else request.end(); });
  }
  async listRoutes(tenant: string): Promise<LiveRoute[]> { const result = completeSinglePage(await this.request("GET", `/internal/v1/model-routes?tenant_external_id=${encodeURIComponent(tenant)}&limit=100`), "target route"); return result.map(parseLiveRoute); }
  async listAccounts(tenant: string): Promise<{ id: string; driver: string; status: string; updatedAt: number }[]> { const result = completeSinglePage(await this.request("GET", `/internal/v1/upstreams?tenant_external_id=${encodeURIComponent(tenant)}&limit=100`), "target upstream"); return result.map((value) => { if (!value || typeof value !== "object" || Array.isArray(value)) throw new RouteImportFailure("target upstream response is invalid"); const item = value as Obj; return { id: text(item.id, "target upstream", UUID), driver: text(item.driver, "target upstream"), status: text(item.status, "target upstream"), updatedAt: integer(item.updated_at, "target upstream") }; }); }
  private payload(tenant: string, spec: RouteSpec): Obj { return { tenant_external_id: tenant, public_model: spec.publicModel, upstream_account_ids: [spec.accountId], upstream_model: spec.upstreamModel, protocol: spec.protocol, priority: spec.priority, included_provider_group_ids: [], excluded_provider_group_ids: [], route_group_ids: [], route_group_names: [], granted_credential_ids: [], custom_model_confirmed: true }; }
  async create(tenant: string, spec: RouteSpec): Promise<LiveRoute> { return parseLiveRoute(await this.request("POST", "/internal/v1/model-routes", this.payload(tenant, spec))); }
  async update(tenant: string, spec: RouteSpec): Promise<LiveRoute> { return parseLiveRoute(await this.request("PUT", `/internal/v1/model-routes/${spec.existing.routeId}`, { ...this.payload(tenant, spec), expected_updated_at: spec.existing.updatedAt, expected_grant_revision: spec.existing.grantRevision })); }
}

export function completeSinglePage(value: unknown, label: string): unknown[] {
  if (!Array.isArray(value)) throw new RouteImportFailure(`${label} response is invalid`);
  if (value.length >= 100) throw new RouteImportFailure(`${label} inventory reached the single-page safety boundary`);
  return value;
}

type Options = { source?: string; upstream?: string; manifest?: string; target?: string; token?: string; checkpoint?: string; apply: boolean; allowHttpTarget: boolean };
function options(argv: string[]): Options {
  if (argv.includes("--help") || argv.includes("-h")) { process.stdout.write("usage: import-cpa-model-routes --source-inventory-file FILE --upstream-inventory-file FILE --reviewed-manifest-file FILE --target-api-base-url URL --service-token-file FILE [--checkpoint-file FILE] [--apply] [--allow-http-target]\n\nProvider-exact CPA route convergence; live dry-run by default. Copilot/Cursor reauthorization is report-only. HTTP requires an exact owner-reviewed manifest URL.\n"); process.exit(0); }
  const output: Options = { apply: false, allowHttpTarget: false }; const names: Record<string, keyof Options> = { "--source-inventory-file": "source", "--upstream-inventory-file": "upstream", "--reviewed-manifest-file": "manifest", "--target-api-base-url": "target", "--service-token-file": "token", "--checkpoint-file": "checkpoint" };
  for (let index = 0; index < argv.length; index++) { const argument = argv[index]!; if (argument === "--apply") output.apply = true; else if (argument === "--allow-http-target") output.allowHttpTarget = true; else { const key = names[argument], value = argv[++index]; if (!key || !value || value.startsWith("--")) throw new RouteImportFailure("arguments are invalid"); (output as Record<string, unknown>)[key] = value; } }
  if (!output.source || !output.upstream || !output.manifest || !output.target || !output.token) throw new RouteImportFailure("required arguments are missing"); return output;
}
function safeBase(raw: string, reviewed: string, allowHttp: boolean): URL { if (raw !== reviewed) throw new RouteImportFailure("target API URL differs from the owner-reviewed manifest"); let url: URL; try { url = new URL(raw); } catch { throw new RouteImportFailure("target API URL is invalid"); } if (url.username || url.password || url.search || url.hash || url.pathname !== "/") throw new RouteImportFailure("target API URL is invalid"); if (url.protocol === "http:" && allowHttp) return url; if (url.protocol !== "https:") throw new RouteImportFailure("HTTP target requires explicit owner-reviewed opt-in"); return url; }

export async function main(argv = process.argv.slice(2)): Promise<number> {
  let token: Buffer | undefined;
  try { const selected = options(argv); const source = readProtected(selected.source!, "source inventory"), upstream = readProtected(selected.upstream!, "upstream inventory"), manifest = readProtected(selected.manifest!, "reviewed manifest"); token = readProtected(selected.token!, "service token"); const tokenText = token.toString("utf8").trim(); if (!TOKEN.test(tokenText)) throw new RouteImportFailure("service token file is invalid"); token.fill(0); token = Buffer.from(tokenText); const reviewed = parseManifest(manifest, digest(source), digest(upstream)); const target = new HttpTarget(safeBase(selected.target!, reviewed.targetBaseUrl, selected.allowHttpTarget), token); const parsed = parseUpstreamInventory(upstream); const plan = createPlan(source, upstream, manifest, await target.listRoutes(parsed.tenant), await target.listAccounts(parsed.tenant)); const counts = await execute(plan, parsed.tenant, target, selected.apply, selected.checkpoint); process.stdout.write(`${encode({ mode: selected.apply ? "apply" : "dry-run", source_inventory_sha256: plan.sourceDigest, upstream_inventory_sha256: plan.upstreamDigest, reviewed_manifest_sha256: plan.manifestDigest, plan_sha256: plan.planDigest, ...counts })}\n`); return 0; }
  catch (error) { const failure = error instanceof RouteImportFailure ? error : new RouteImportFailure("route importer failed"); process.stderr.write(`${encode({ error: failure.message, ...(failure.counts ?? {}) })}\n`); return 1; }
  finally { token?.fill(0); }
}
if (process.argv[1] && import.meta.url === new URL(`file://${process.argv[1]}`).href) process.exitCode = await main();
