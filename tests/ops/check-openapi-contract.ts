#!/usr/bin/env node
/** Fail when Axum routes, OpenAPI operations, or role boundaries diverge. */

import SwaggerParser from "@apidevtools/swagger-parser";
import { isDeepStrictEqual } from "node:util";
import { mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { dirname, relative, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

const HTTP_METHODS = new Set(["delete", "get", "head", "options", "patch", "post", "put", "trace"]);
const ROUTE_METHOD = /(?<![A-Za-z0-9_])(delete|get|head|options|patch|post|put|trace)\s*\(/gu;
const EXPECTED_RUNTIME_ROLES: Record<string, string[]> = { common: ["gateway", "control", "worker", "all"], control: ["control", "all"], gateway: ["gateway", "all"] };
type Obj = Record<string, any>;

export class ContractFailure extends Error {}

function object(value: unknown): value is Obj { return typeof value === "object" && value !== null && !Array.isArray(value); }
function setEqual<T>(left: Iterable<T>, right: Iterable<T>): boolean { const a = new Set(left); const b = new Set(right); return a.size === b.size && [...a].every((item) => b.has(item)); }
function subset<T>(left: Iterable<T>, right: Iterable<T>): boolean { const b = new Set(right); return [...left].every((item) => b.has(item)); }

export function rustCharLiteralAt(source: string, index: number): boolean {
  if (source[index] !== "'" || index + 2 >= source.length) return false;
  if (source[index + 1] === "\\") { const closing = source.indexOf("'", index + 2); return closing - index > 0 && closing - index <= 12; }
  return source[index + 2] === "'";
}

export function balancedSlice(source: string, start: number, opening: string, closing: string): [string, number] {
  if (source[start] !== opening) throw new ContractFailure(`expected ${JSON.stringify(opening)} at byte ${start}`);
  let depth = 0; let quote: string | undefined; let escaped = false; let lineComment = false; let blockComment = 0;
  for (let index = start; index < source.length;) {
    const char = source[index]!; const following = source[index + 1] ?? "";
    if (lineComment) { if (char === "\n") lineComment = false; index += 1; continue; }
    if (blockComment) { if (char === "/" && following === "*") { blockComment += 1; index += 2; continue; } if (char === "*" && following === "/") { blockComment -= 1; index += 2; continue; } index += 1; continue; }
    if (quote !== undefined) { if (escaped) escaped = false; else if (char === "\\") escaped = true; else if (char === quote) quote = undefined; index += 1; continue; }
    if (char === "/" && following === "/") { lineComment = true; index += 2; continue; }
    if (char === "/" && following === "*") { blockComment = 1; index += 2; continue; }
    if (char === '"' || (char === "'" && rustCharLiteralAt(source, index))) { quote = char; index += 1; continue; }
    if (char === opening) depth += 1;
    else if (char === closing && --depth === 0) return [source.slice(start + 1, index), index + 1];
    index += 1;
  }
  throw new ContractFailure(`unbalanced ${opening}${closing} beginning at byte ${start}`);
}

function functionBody(source: string, functionName: string): string {
  const match = new RegExp(`\\bfn\\s+${functionName.replace(/[.*+?^${}()|[\]\\]/gu, "\\$&")}\\s*\\(`, "u").exec(source);
  if (match === null) throw new ContractFailure(`Rust function ${functionName} was not found`);
  const brace = source.indexOf("{", match.index + match[0].length);
  if (brace < 0) throw new ContractFailure(`Rust function ${functionName} has no body`);
  return balancedSlice(source, brace, "{", "}")[0];
}

export function codeMask(source: string): boolean[] {
  const mask = Array<boolean>(source.length).fill(false); let quote: string | undefined; let escaped = false; let lineComment = false; let blockComment = 0;
  for (let index = 0; index < source.length;) {
    const char = source[index]!; const following = source[index + 1] ?? "";
    if (lineComment) { if (char === "\n") { lineComment = false; mask[index] = true; } index += 1; continue; }
    if (blockComment) { if (char === "/" && following === "*") { blockComment += 1; index += 2; continue; } if (char === "*" && following === "/") { blockComment -= 1; index += 2; continue; } index += 1; continue; }
    if (quote !== undefined) { if (escaped) escaped = false; else if (char === "\\") escaped = true; else if (char === quote) quote = undefined; index += 1; continue; }
    if (char === "/" && following === "/") { lineComment = true; index += 2; continue; }
    if (char === "/" && following === "*") { blockComment = 1; index += 2; continue; }
    if (char === '"' || (char === "'" && rustCharLiteralAt(source, index))) { quote = char; index += 1; continue; }
    mask[index] = true; index += 1;
  }
  return mask;
}

function failOnUnparsedRouterComposition(body: string, functionName: string, mask: boolean[]): void {
  for (const unsupported of [".fallback", ".fallback_service", ".nest", ".nest_service", ".route_service"]) {
    for (let cursor = 0;;) { const marker = body.indexOf(unsupported, cursor); if (marker < 0) break; if (mask[marker]) throw new ContractFailure(`${functionName} uses unsupported Router composition ${unsupported}; extend the contract extractor before adding it`); cursor = marker + unsupported.length; }
  }
  for (let cursor = 0;;) {
    const marker = body.indexOf(".merge", cursor); if (marker < 0) return;
    if (!mask[marker]) { cursor = marker + 6; continue; }
    let opening = marker + 6; while (/\s/u.test(body[opening] ?? "")) opening += 1;
    if (body[opening] !== "(") { cursor = opening; continue; }
    const [argumentsText, end] = balancedSlice(body, opening, "(", ")"); const expression = argumentsText.trim();
    if (expression !== "authenticated" && !expression.startsWith("control_router(") && !expression.startsWith("gateway_router(")) throw new ContractFailure(`${functionName} merges an unparsed Router expression: ${expression.slice(0, 120)}`);
    cursor = end;
  }
}

function controlGuardRanges(body: string, functionName: string): Array<[number, number]> {
  if (functionName !== "router_for_role") return [];
  const guard = /if\s+matches!\(\s*role\s*,\s*RuntimeRole::Control\s*\|\s*RuntimeRole::All\s*\)\s*\{/gu;
  const ranges: Array<[number, number]> = [];
  for (const match of body.matchAll(guard)) { const opening = body.lastIndexOf("{", (match.index ?? 0) + match[0].length); ranges.push([opening, balancedSlice(body, opening, "{", "}")[1]]); }
  if (ranges.length !== 1) throw new ContractFailure("router_for_role must retain one explicit Control|All guard for private system routes");
  return ranges;
}

function duplicateKeys(items: Obj[], fields: string[]): string[] {
  const seen = new Set<string>(); const duplicates = new Set<string>();
  for (const item of items) { const key = fields.map((field) => String(item[field])).join(" "); if (seen.has(key)) duplicates.add(key); seen.add(key); }
  return [...duplicates].sort();
}

export function sourceRoutes(source: string): Obj[] {
  const routes: Obj[] = [];
  for (const [role, functionName] of [["common", "router_for_role"], ["control", "control_router"], ["gateway", "gateway_router"]] as const) {
    const body = functionBody(source, functionName); const mask = codeMask(body); failOnUnparsedRouterComposition(body, functionName, mask); const guards = controlGuardRanges(body, functionName);
    for (let cursor = 0;;) {
      const marker = body.indexOf(".route", cursor); if (marker < 0) break;
      if (!mask[marker]) { cursor = marker + 6; continue; }
      let opening = marker + 6; while (/\s/u.test(body[opening] ?? "")) opening += 1;
      if (body[opening] !== "(") { cursor = opening; continue; }
      const [argumentsText, end] = balancedSlice(body, opening, "(", ")"); const pathMatch = /^\s*"([^"\\]+)"\s*,/u.exec(argumentsText);
      if (pathMatch === null) throw new ContractFailure(`${functionName} contains a .route call without a literal path`);
      const path = pathMatch[1]!; const effectiveRole = role === "common" && guards.some(([start, finish]) => start < marker && marker < finish) ? "control" : role;
      const handler = argumentsText.slice(pathMatch[0].length); const handlerMask = codeMask(handler); const methods = [...new Set([...handler.matchAll(ROUTE_METHOD)].filter((match) => handlerMask[match.index ?? 0]).map((match) => match[1]!))].sort();
      if (methods.length === 0) throw new ContractFailure(`source route ${path} has no recognized HTTP method`);
      routes.push(...methods.map((method) => ({ method, path, source_role: effectiveRole }))); cursor = end;
    }
  }
  const duplicates = duplicateKeys(routes, ["method", "path"]); if (duplicates.length) throw new ContractFailure(`duplicate source method/path routes: ${JSON.stringify(duplicates)}`);
  return routes.sort((a, b) => `${a.path}\0${a.method}`.localeCompare(`${b.path}\0${b.method}`));
}

function resolvePointer(document: Obj, pointer: string): unknown {
  if (!pointer.startsWith("#/")) throw new ContractFailure(`only local OpenAPI references are permitted: ${pointer}`);
  let current: unknown = document;
  for (const raw of pointer.slice(2).split("/")) { const part = raw.replaceAll("~1", "/").replaceAll("~0", "~"); if (!object(current) || !(part in current)) throw new ContractFailure(`unresolvable OpenAPI reference: ${pointer}`); current = current[part]; }
  return current;
}
function references(value: unknown): string[] { if (Array.isArray(value)) return value.flatMap(references); if (!object(value)) return []; return [...(typeof value.$ref === "string" ? [value.$ref] : []), ...Object.values(value).flatMap(references)]; }

export function openapiRoutes(document: Obj): Obj[] {
  if (!object(document.paths)) throw new ContractFailure("OpenAPI paths must be an object"); const routes: Obj[] = [];
  for (const [path, item] of Object.entries(document.paths)) {
    if (!object(item)) throw new ContractFailure("OpenAPI path items must be objects");
    for (const [rawMethod, operation] of Object.entries(item)) { const method = rawMethod.toLowerCase(); if (!HTTP_METHODS.has(method)) continue; if (!object(operation)) throw new ContractFailure(`${method.toUpperCase()} ${path} operation must be an object`); if (typeof operation.operationId !== "string" || !operation.operationId) throw new ContractFailure(`${method.toUpperCase()} ${path} has no operationId`); if (!object(operation.responses) || Object.keys(operation.responses).length === 0) throw new ContractFailure(`${operation.operationId} has no response contract`); const schemaRefs = [...new Set(references(operation))].sort(); for (const ref of schemaRefs) resolvePointer(document, ref); routes.push({ method, path, operation_id: operation.operationId, schema_refs: schemaRefs }); }
  }
  const duplicates = duplicateKeys(routes, ["method", "path"]); const ids = duplicateKeys(routes, ["operation_id"]); if (duplicates.length || ids.length) throw new ContractFailure(`duplicate OpenAPI routes=${JSON.stringify(duplicates)}, operationIds=${JSON.stringify(ids)}`);
  return routes.sort((a, b) => `${a.path}\0${a.method}`.localeCompare(`${b.path}\0${b.method}`));
}

function operationAt(document: Obj, method: string, path: string): Obj { const operation = document.paths?.[path]?.[method]; if (!object(operation)) throw new ContractFailure(`required OpenAPI operation is missing: ${method.toUpperCase()} ${path}`); return operation; }
function parameterReferences(operation: Obj): Set<string> { if (!Array.isArray(operation.parameters ?? [])) throw new ContractFailure("OpenAPI operation parameters must be an array"); return new Set((operation.parameters ?? []).filter(object).map((item: Obj) => item.$ref).filter((item: unknown): item is string => typeof item === "string")); }

function validateGroupContracts(document: Obj): void {
  const families: Record<string, [string, string]> = { provider: ["routes:read", "routes:write"], route: ["routes:read", "routes:write"], credential: ["keys:read", "keys:write"] };
  for (const [family, [readScope, writeScope]] of Object.entries(families)) {
    const collection = `/internal/v1/${family}-groups`; const resource = `${collection}/{group_id}`; const members = `${resource}/members`;
    const operations: Array<[string, string, string]> = [["get", collection, readScope], ["post", collection, writeScope], ["put", resource, writeScope], ["delete", resource, writeScope], ["put", members, writeScope]];
    for (const [method, path, expectedScope] of operations) { const operation = operationAt(document, method, path); if (!isDeepStrictEqual(operation.security, [{ serviceBearer: [] }])) throw new ContractFailure(`${method.toUpperCase()} ${path} group security changed`); if (operation["x-required-scope"] !== expectedScope) throw new ContractFailure(`${method.toUpperCase()} ${path} group scope changed`); if ((method === "put" || method === "delete") && !("409" in (operation.responses ?? {}))) throw new ContractFailure(`${method.toUpperCase()} ${path} lost optimistic-concurrency 409`); if (family === "credential" && operation["x-routing-effect"] !== "none") throw new ContractFailure(`${method.toUpperCase()} ${path} must declare credential groups presentation-only`); }
    if (!setEqual(parameterReferences(operationAt(document, "get", collection)), ["#/components/parameters/RequiredTenant"])) throw new ContractFailure(`GET ${collection} must require an explicit tenant`);
    if (!setEqual(parameterReferences(operationAt(document, "delete", resource)), ["#/components/parameters/GroupId", "#/components/parameters/RequiredTenant", "#/components/parameters/ExpectedUpdatedAt"])) throw new ContractFailure(`DELETE ${resource} tenant/CAS parameters changed`);
  }
  const schemas = document.components?.schemas ?? {}; for (const name of ["CreateRoutingGroupRequest", "ReplaceRoutingGroupRequest", "ReplaceRoutingGroupMembersRequest"]) if (name in schemas) throw new ContractFailure(`group schema name is not neutral: ${name}`);
  const responses = document.components?.responses ?? {}; for (const name of ["RoutingGroupList", "RoutingGroupCreated", "RoutingGroupUpdated"]) if (name in responses) throw new ContractFailure(`group response name is not neutral: ${name}`);
  const enriched = ["upstream_account_ids", "included_provider_group_ids", "excluded_provider_group_ids", "route_group_ids", "granted_credential_ids", "candidate_upstream_account_ids", "custom_model_confirmed", "grant_revision"];
  if (!subset(enriched, schemas.ModelRoute?.required ?? [])) throw new ContractFailure("model-route lists must return enriched associations and CAS");
  if ("allow_unverified_custom_model" in (schemas.ModelRoute?.properties ?? {})) throw new ContractFailure("model route exposed the retired custom-model field name");
  for (const name of ["CreateModelRouteRequest", "ReplaceModelRouteRequest", "ReplaceModelRouteRoutingRequest"]) { const properties = schemas[name]?.properties ?? {}; if (!("custom_model_confirmed" in properties)) throw new ContractFailure(`${name} lost explicit custom-model acknowledgement`); if ("credential_group_ids" in properties) throw new ContractFailure(`${name} must not accept credential groups as grants`); }
  for (const name of ["ReplaceModelRouteRequest", "ReplaceModelRouteRoutingRequest"]) if (!subset(["expected_updated_at", "expected_grant_revision"], schemas[name]?.required ?? [])) throw new ContractFailure(`${name} lost base/relation compare-and-set tokens`);
  if (!(schemas.ClientCredentialRouting?.required ?? []).includes("grant_revision")) throw new ContractFailure("credential routing response lost direct-grant revision");
  if (!setEqual(schemas.ReplaceClientCredentialRoutingRequest?.required ?? [], ["tenant_external_id", "route_ids", "route_group_ids", "expected_grant_revision"])) throw new ContractFailure("credential routing replacement must use only direct-grant CAS");
  const credentialReplace = schemas.ReplaceClientCredentialRoutingRequest?.properties ?? {}; if ("expected_updated_at" in credentialReplace) throw new ContractFailure("credential grant writes must not couple to key metadata CAS"); if ("credential_group_ids" in credentialReplace) throw new ContractFailure("credential groups must never be accepted by routing grants");
  const keyCreate = schemas.CreateClientCredentialRequest?.properties ?? {}; if (!("route_ids" in keyCreate) || !("route_group_ids" in keyCreate)) throw new ContractFailure("credential creation lost atomic initial route grants"); if ("credential_group_ids" in keyCreate) throw new ContractFailure("credential creation must not treat credential groups as grants");
  const expectedRouting = { "provider-candidates": "explicit-accounts-union-included-provider-groups-minus-excluded-provider-groups", "provider-group-exclusion-wins": true, "route-groups-authorize-credentials": true, "credential-groups-affect-routing": false };
  if (!isDeepStrictEqual(operationAt(document, "get", "/internal/v1/model-routes/{route_id}/routing")["x-routing-contract"], expectedRouting)) throw new ContractFailure("model route provider/group semantics changed");
  const relevant = JSON.stringify({ paths: Object.fromEntries(Object.entries(document.paths ?? {}).filter(([path]) => path.includes("-groups") || path.endsWith("/routing"))), schemas: Object.fromEntries(Object.entries(schemas).filter(([name]) => name.includes("Group") || name.includes("Routing") || name === "ModelRoute")) }).toLowerCase();
  for (const retired of ["provider tag", "route tag", "credential tag", "provider pool", "route pool", "credential pool", "rule group", "allow_unverified_custom_model"]) if (relevant.includes(retired)) throw new ContractFailure(`group contract contains retired term: ${retired}`);
}

export function validateProductContracts(document: Obj): void {
  validateGroupContracts(document); const schemas = document.components?.schemas ?? {};
  for (const [method, path, security, scope] of [["patch", "/internal/v1/keys/{key_id}/alias", [{ serviceBearer: [] }], "keys:write"], ["get", "/internal/v1/keys/{key_id}/limits", [{ serviceBearer: [] }], "keys:read"], ["get", "/self/v1/key/limits", [{ clientBearer: [] }], undefined]] as const) { const operation = operationAt(document, method, path); if (!isDeepStrictEqual(operation.security, security)) throw new ContractFailure(`${method.toUpperCase()} ${path} credential security changed`); if (operation["x-required-scope"] !== scope) throw new ContractFailure(`${method.toUpperCase()} ${path} credential scope changed`); const schema = operation.responses?.["200"]?.content?.["application/json"]?.schema; if (path.endsWith("/limits") && schema?.$ref !== "#/components/schemas/ClientCredentialLimitSnapshot") throw new ContractFailure(`${method.toUpperCase()} ${path} limit snapshot schema changed`); }
  const legacy = operationAt(document, "post", "/internal/v1/keys/{key_id}/legacy-credentials"); if (!("409" in (legacy.responses ?? {}))) throw new ContractFailure("legacy credential one-to-one conflicts must retain HTTP 409"); const legacyProperties = legacy.requestBody?.content?.["application/json"]?.schema?.properties ?? {}; if (legacyProperties.credential?.minLength !== 16 || legacyProperties.credential?.maxLength !== 512) throw new ContractFailure("legacy credential runtime length contract changed"); if (legacyProperties.source_hash?.minLength !== 64 || legacyProperties.source_hash?.maxLength !== 64) throw new ContractFailure("legacy credential source hash must remain an exact digest");
  const policy = schemas.CredentialPolicy?.properties ?? {}; if (policy.tokens_per_minute?.maximum !== 9_007_199_254_740_991) throw new ContractFailure("credential TPM must remain JSON-safe"); if ("allowed_models" in policy) throw new ContractFailure("legacy allowed_models leaked into the public credential policy");
  const selfUsage = operationAt(document, "get", "/self/v1/usage-analysis"); if (!isDeepStrictEqual(selfUsage.security, [{ clientBearer: [] }])) throw new ContractFailure("GET /self/v1/usage-analysis must retain client credential security"); if (!setEqual(parameterReferences(selfUsage), ["#/components/parameters/FromCreatedAt", "#/components/parameters/ToCreatedAt", "#/components/parameters/ModelFilter", "#/components/parameters/ErrorCodeFilter"])) throw new ContractFailure("self usage analysis accepted a management selector or lost a safe filter"); if (selfUsage.responses?.["200"]?.headers?.["Cache-Control"]?.schema?.const !== "private, no-store") throw new ContractFailure("GET /self/v1/usage-analysis lost private no-store caching"); const selfUsageSchema = schemas.SelfUsageAnalysis ?? {}; if (selfUsageSchema.additionalProperties !== false) throw new ContractFailure("SelfUsageAnalysis must reject undeclared identity dimensions"); for (const forbidden of ["tenant", "tenant_external_id", "key", "key_id", "by_key", "principal", "route", "route_id", "session", "by_session", "upstream", "upstream_grouping", "by_upstream"]) if (forbidden in (selfUsageSchema.properties ?? {})) throw new ContractFailure(`SelfUsageAnalysis leaked operator-only property ${forbidden}`);
  if (!("Retry-After" in (document.components?.responses?.UsageRejected?.headers ?? {}))) throw new ContractFailure("usage rejection lost Retry-After contract"); const errorSchema = schemas.UsageLimitErrorResponse?.properties?.error ?? {}; if (!subset(["code", "message", "reason", "retryable"], errorSchema.required ?? [])) throw new ContractFailure("usage rejection lost fixed reason/retryable fields"); if (!setEqual(errorSchema.properties?.reason?.enum ?? [], ["balance_exhausted", "daily_budget_exhausted", "weekly_budget_exhausted", "lifetime_budget_exhausted", "rpm_exhausted", "tpm_exhausted", "concurrency_exhausted"])) throw new ContractFailure("usage rejection reason enum changed");
  const assets: Array<[string, Obj[], string | undefined, string[], Obj, string[]]> = [
    ["/internal/v1/requests/{request_id}/assets/{asset_id}", [{ serviceBearer: [] }], "requests:read", ["#/components/parameters/RequestIdPath", "#/components/parameters/AssetIdPath", "#/components/parameters/TenantFilter", "#/components/parameters/OptionalByteRange"], { identity: "service-credential", "tenant-isolation-failure": 404 }, ["200", "206", "401", "403", "404", "416", "500"]],
    ["/self/v1/requests/{request_id}/assets/{asset_id}", [{ clientBearer: [] }], undefined, ["#/components/parameters/RequestIdPath", "#/components/parameters/AssetIdPath", "#/components/parameters/OptionalByteRange"], { identity: "stable-key-id", "ownership-failure": 404 }, ["200", "206", "401", "404", "416", "500"]],
  ];
  for (const [path, security, scope, parameters, authorization, responses] of assets) { const operation = operationAt(document, "get", path); if (!isDeepStrictEqual(operation.security, security)) throw new ContractFailure(`GET ${path} must retain ${JSON.stringify(security)} security`); if (operation["x-required-scope"] !== scope) throw new ContractFailure(`GET ${path} must retain scope ${String(scope)}`); if (!setEqual(parameterReferences(operation), parameters)) throw new ContractFailure(`GET ${path} asset/tenant/range parameters changed`); if (!subset(responses, Object.keys(operation.responses ?? {}))) throw new ContractFailure(`GET ${path} must retain responses ${JSON.stringify([...responses].sort())}`); for (const [status, ref] of Object.entries({ "200": "#/components/responses/GenerationAssetFull", "206": "#/components/responses/GenerationAssetPartial", "404": "#/components/responses/NotFound", "416": "#/components/responses/RangeNotSatisfiable", "500": "#/components/responses/InternalError" })) if (!isDeepStrictEqual(operation.responses?.[status], { $ref: ref })) throw new ContractFailure(`GET ${path} response ${status} contract changed`); const actual = operation["x-authorization-contract"]; if (!object(actual) || Object.entries(authorization).some(([key, value]) => actual[key] !== value)) throw new ContractFailure(`GET ${path} ownership/tenant isolation contract changed`); if (actual["object-binding"] !== "request-id-and-asset-id") throw new ContractFailure(`GET ${path} must bind both opaque identifiers`); if (actual["archive-locator-exposed"] !== false) throw new ContractFailure(`GET ${path} must not expose archive locators`); }
  const image = operationAt(document, "post", "/v1/images/generations"); if (!isDeepStrictEqual(image.security, [{ clientBearer: [] }])) throw new ContractFailure("POST /v1/images/generations must retain client security"); if (!parameterReferences(image).has("#/components/parameters/OptionalIdempotencyKey")) throw new ContractFailure("POST /v1/images/generations lost optional Idempotency-Key"); if (!subset(["200", "202", "400", "401", "403", "409", "424", "429", "502"], Object.keys(image.responses ?? {}))) throw new ContractFailure("POST /v1/images/generations response contract regressed"); const idempotency = image["x-idempotency-contract"];
  const requiredIdempotency = { namespace: "stable-key-id", fingerprint: "post-policy-canonical-model-and-request", "completed-replay": "exact-status-body-and-request-id", "different-payload-status": 400, "in-progress-status": 409, "expired-pending": "atomically-reclaimed", "persisted-url-results": "mtc-request-asset-reference-only" }; if (!object(idempotency) || Object.entries(requiredIdempotency).some(([key, value]) => idempotency[key] !== value)) throw new ContractFailure("POST /v1/images/generations idempotency contract changed"); if (!setEqual(idempotency["completed-replay-effects"] ?? [], ["no-request-record", "no-upstream-request", "no-quota-reservation", "no-usage-settlement", "no-key-rate-limit-window-consumption", "no-image-execution-permit"])) throw new ContractFailure("completed synchronous image replay acquired side effects"); if (!setEqual(idempotency["replay-preconditions"] ?? [], ["authentication", "traffic-policy-evaluation"])) throw new ContractFailure("synchronous image replay preconditions changed"); if (!setEqual(idempotency["fresh-execution-only"] ?? [], ["route-resolution", "price-lookup", "outbound-client-validation"])) throw new ContractFailure("completed image replay regained mutable route/price dependencies");
  const cloud = operationAt(document, "put", "/internal/v1/integrations/memeloop-cloud/subscription"); if (!isDeepStrictEqual(cloud.security, [{ memeloopCloudHmac: [] }])) throw new ContractFailure("MemeLoop Cloud subscription sync lost HMAC security"); const named = new Set((cloud.parameters ?? []).filter(object).map((parameter: Obj) => parameter.name).filter(Boolean)); if (!setEqual(parameterReferences(cloud), ["#/components/parameters/RequiredIdempotencyKey"]) || !setEqual(named, ["X-MTC-Webhook-Timestamp", "X-MTC-Webhook-Signature"])) throw new ContractFailure("MemeLoop Cloud signature/idempotency headers changed"); if (!subset(["200", "201", "400", "401", "403", "404", "409"], Object.keys(cloud.responses ?? {}))) throw new ContractFailure("MemeLoop Cloud lifecycle response contract regressed"); if (!isDeepStrictEqual(cloud["x-signature-contract"], { algorithm: "HMAC-SHA-256", encoding: "base64url-no-padding", envelope: "ascii-timestamp-dot-exact-body", "tolerance-seconds": 300, "disabled-without-secret": true })) throw new ContractFailure("MemeLoop Cloud signature contract changed");
  const cloudIdempotency = { namespace: "tenant-and-event-id-digest", payload: "canonical-full-snapshot", "different-payload-status": 409, "stale-version-status": 409, "quota-version-source": "durable-subscription-entitlement", "policy-update": "compare-and-set-on-current-entitlement-version", "routing-update": "same-transaction-normalized-grants", "legacy-allowed-models": "current-route-snapshot-only", "stable-history-owner": "key-id-and-account-id", "raw-event-id-persisted": false }; if (!isDeepStrictEqual(cloud["x-idempotency-contract"], cloudIdempotency)) throw new ContractFailure("MemeLoop Cloud ordered idempotency contract changed"); const snapshot = schemas.MemeLoopCloudSubscriptionSnapshot ?? {}; if (snapshot.additionalProperties !== false || !setEqual(snapshot.properties?.status?.enum ?? [], ["active", "cancelled"])) throw new ContractFailure("MemeLoop Cloud full snapshot schema changed");
}

function matchingRule(path: string, boundary: Obj): Obj { const matches = boundary.rules.filter((rule: Obj) => (rule.exact_paths ?? []).includes(path) || (rule.path_prefixes ?? []).some((prefix: string) => path.startsWith(prefix))); if (matches.length !== 1) throw new ContractFailure(`route ${path} matched ${matches.length} boundary rules: ${JSON.stringify(matches.map((rule: Obj) => rule.name ?? "<unnamed>"))}`); return matches[0]!; }

export function checkContract(source: Obj[], specRoutes: Obj[], spec: Obj, boundary: Obj): Obj {
  validateProductContracts(spec); const sourceMap = new Map(source.map((route) => [`${route.method} ${route.path}`, route])); const specMap = new Map(specRoutes.map((route) => [`${route.method} ${route.path}`, route])); const contract: Obj[] = [];
  for (const [key, sourceRoute] of sourceMap) { const rule = matchingRule(sourceRoute.path, boundary); if (rule.source_role !== sourceRoute.source_role) throw new ContractFailure(`${key} is registered in ${sourceRoute.source_role} but boundary says ${rule.source_role}`); const runtimeRoles = EXPECTED_RUNTIME_ROLES[sourceRoute.source_role]!; if (!isDeepStrictEqual(rule.runtime_roles, runtimeRoles)) throw new ContractFailure(`boundary ${rule.name} runtime_roles=${JSON.stringify(rule.runtime_roles)} but source role ${sourceRoute.source_role} requires ${JSON.stringify(runtimeRoles)}`); const expected = Boolean(rule.in_openapi); const actual = specMap.has(key); if (expected !== actual) throw new ContractFailure(`${key} OpenAPI presence is ${actual}, boundary requires ${expected}`); const item: Obj = { ...sourceRoute, boundary: rule.name, runtime_roles: rule.runtime_roles, exposure: rule.exposure, authentication: rule.authentication, in_openapi: actual }; if (actual) { const operation = specMap.get(key)!; item.operation_id = operation.operation_id; item.schema_refs = operation.schema_refs; } contract.push(item); }
  const missing = [...specMap.keys()].filter((key) => !sourceMap.has(key)).sort(); if (missing.length) throw new ContractFailure(`OpenAPI operations missing from Axum source: ${JSON.stringify(missing)}`); const pathCount = Object.keys(spec.paths).length; const operationCount = specRoutes.length; if (pathCount < Number(boundary.minimum_openapi_paths) || operationCount < Number(boundary.minimum_openapi_operations)) throw new ContractFailure(`OpenAPI surface regressed: paths=${pathCount} (minimum ${boundary.minimum_openapi_paths}), operations=${operationCount} (minimum ${boundary.minimum_openapi_operations})`); const violations = [...new Set(contract.filter((item) => item.source_role === "control" && item.exposure !== "private-only").map((item) => item.path))].sort(); if (violations.length) throw new ContractFailure(`control routes are not private-only: ${JSON.stringify(violations)}`);
  return { schema_version: 1, openapi_paths: pathCount, openapi_operations: operationCount, source_operations: source.length, excluded_source_operations: source.length - operationCount, routes: contract.sort((a, b) => `${a.path}\0${a.method}`.localeCompare(`${b.path}\0${b.method}`)) };
}

function rustFiles(path: string): string[] { if (statSync(path).isFile()) return [path]; if (!statSync(path).isDirectory()) return []; return readdirSync(path, { withFileTypes: true }).flatMap((entry) => entry.isDirectory() ? rustFiles(resolve(path, entry.name)) : entry.name.endsWith(".rs") ? [resolve(path, entry.name)] : []).sort(); }
export function readRustSource(path: string): string { let sources: string[]; try { sources = rustFiles(path); } catch { throw new ContractFailure(`Rust API source does not exist: ${path}`); } if (!sources.length) throw new ContractFailure(`Rust API source directory is empty: ${path}`); if (sources.length === 1 && statSync(path).isFile()) return readFileSync(path, "utf8"); return sources.map((source) => `// source: ${relative(path, source).replaceAll("\\", "/")}\n${readFileSync(source, "utf8")}`).join("\n"); }

interface Arguments { source: string; openapi: string; boundaries: string; output?: string }
function parseArgs(argv: string[]): Arguments { const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../.."); const args: Arguments = { source: `${repository}/src/api`, openapi: `${repository}/openapi/openapi.yaml`, boundaries: `${repository}/openapi/route-boundaries.json` }; for (let index = 0; index < argv.length; index += 2) { const flag = argv[index]; const value = argv[index + 1]; if (flag === "--help" || flag === "-h") { console.log("Usage: check-openapi-contract.ts [--source PATH] [--openapi PATH] [--boundaries PATH] [--output PATH]"); process.exit(0); } if (!value) throw new ContractFailure(`${flag} requires a path`); if (flag === "--source") args.source = value; else if (flag === "--openapi") args.openapi = value; else if (flag === "--boundaries") args.boundaries = value; else if (flag === "--output") args.output = value; else throw new ContractFailure(`unrecognized argument: ${flag}`); } return args; }

export async function main(argv = process.argv.slice(2)): Promise<number> {
  try { const args = parseArgs(argv); const source = readRustSource(args.source); const spec = parse(readFileSync(args.openapi, "utf8"), { uniqueKeys: true }) as unknown; const boundary = JSON.parse(readFileSync(args.boundaries, "utf8")) as unknown; if (!object(spec) || !object(boundary)) throw new ContractFailure("OpenAPI and boundary documents must be objects"); await SwaggerParser.validate(args.openapi, { resolve: { external: false } }); const report = checkContract(sourceRoutes(source), openapiRoutes(spec), spec, boundary); const encoded = `${JSON.stringify(report, null, 2)}\n`; if (args.output) { mkdirSync(dirname(args.output), { recursive: true }); writeFileSync(args.output, encoded); } console.log(`OpenAPI contract OK: ${report.openapi_paths} paths, ${report.openapi_operations} operations, ${report.excluded_source_operations} intentional source-only static routes`); return 0; }
  catch (error) { console.error(`OpenAPI contract FAILED: ${error instanceof Error ? error.message : String(error)}`); return 1; }
}

if (import.meta.url === `file://${process.argv[1]}`) process.exitCode = await main();
