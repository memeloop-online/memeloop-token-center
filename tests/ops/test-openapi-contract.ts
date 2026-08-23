#!/usr/bin/env node
/** Focused regression tests for the Rust route extractor and product contract. */

import assert from "node:assert/strict";
import { mkdtempSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";
import { parse } from "yaml";
import { ContractFailure, readRustSource, sourceRoutes, validateProductContracts } from "./check-openapi-contract.ts";

type Obj = Record<string, any>;
const repository = resolve(dirname(fileURLToPath(import.meta.url)), "../..");
const originalDocument = parse(readFileSync(`${repository}/openapi/openapi.yaml`, "utf8")) as Obj;
const cloneDocument = (): Obj => structuredClone(originalDocument) as Obj;

function sourceWith(controlExtra = "", gatewayExtra = "", commonExtra = ""): string {
  return `
fn router_for_role(state: AppState, role: RuntimeRole) -> Router {
    let mut application = Router::new()
        .route("/livez", get(liveness));
    // .route("/ghost", get(comment_must_not_be_parsed));
    if matches!(role, RuntimeRole::Control | RuntimeRole::All) {
        application = application.route("/metrics", get(metrics));
    }
    application = application.merge(control_router(state.clone()));${commonExtra}
    application = application.merge(gateway_router(state));
    application
}
fn control_router(state: AppState) -> Router<AppState> {
    Router::new().route("/internal/v1/example", get(example))${controlExtra}
}
fn gateway_router(state: AppState) -> Router<AppState> {
    Router::new().route("/v1/example", get(example))${gatewayExtra}
}`;
}

test("source directory is combined in stable relative path order", () => {
  const root = mkdtempSync(`${tmpdir()}/mtc-route-contract-`); mkdirSync(`${root}/nested`); writeFileSync(`${root}/z.rs`, "fn z() {}\n"); writeFileSync(`${root}/nested/a.rs`, "fn a() {}\n");
  const source = readRustSource(root); assert.ok(source.indexOf("nested/a.rs") < source.indexOf("z.rs"));
});

test("comments are ignored and control guard is classified", () => {
  const routes = sourceRoutes(sourceWith()); assert.ok(!routes.some((route) => route.path === "/ghost")); assert.equal(routes.find((route) => route.path === "/metrics")?.source_role, "control"); assert.equal(routes.length, 4);
});
test("route_service fails closed", () => assert.throws(() => sourceRoutes(sourceWith("", '.route_service("/opaque", service)')), /route_service/u));
test("handler comments do not add methods", () => assert.deepEqual(sourceRoutes(sourceWith("", '.route("/v1/comment", post(handler) \/\* get(fake) \*\/)')).filter((route) => route.path === "/v1/comment").map((route) => route.method), ["post"]));
test("fallback fails closed", () => assert.throws(() => sourceRoutes(sourceWith("", ".fallback(handler)")), /fallback/u));
test("lifetime apostrophe is not parsed as a character literal", () => assert.equal(sourceRoutes(sourceWith("", "", ".layer(foo::<'static>())")).length, 4));
test("unknown merged router fails closed", () => assert.throws(() => sourceRoutes(sourceWith(".merge(helper_router())")), /unparsed Router/u));

test("asset and image semantics are complete", () => validateProductContracts(cloneDocument()));
test("self limit snapshot security regression fails closed", () => { const document = cloneDocument(); document.paths["/self/v1/key/limits"].get.security = [{ serviceBearer: [] }]; assert.throws(() => validateProductContracts(document), /credential security/u); });
test("usage limit reason regression fails closed", () => { const document = cloneDocument(); const required: string[] = document.components.schemas.UsageLimitErrorResponse.properties.error.required; required.splice(required.indexOf("retryable"), 1); assert.throws(() => validateProductContracts(document), /reason\/retryable/u); });
test("asset range response regression fails closed", () => { const document = cloneDocument(); delete document.paths["/self/v1/requests/{request_id}/assets/{asset_id}"].get.responses["416"]; assert.throws(() => validateProductContracts(document), /must retain responses/u); });
test("synchronous image namespace regression fails closed", () => { const document = cloneDocument(); document.paths["/v1/images/generations"].post["x-idempotency-contract"].namespace = "raw-client-secret"; assert.throws(() => validateProductContracts(document), /idempotency contract/u); });
test("cloud subscription policy rollback regression fails closed", () => { const document = cloneDocument(); document.paths["/internal/v1/integrations/memeloop-cloud/subscription"].put["x-idempotency-contract"]["policy-update"] = "unversioned"; assert.throws(() => validateProductContracts(document), /Cloud ordered idempotency contract/u); });
test("cloud subscription hmac regression fails closed", () => { const document = cloneDocument(); document.paths["/internal/v1/integrations/memeloop-cloud/subscription"].put.security = [{ serviceBearer: [] }]; assert.throws(() => validateProductContracts(document), /lost HMAC security/u); });

test("usage analysis contract is currency safe and canonical", () => {
  const document = cloneDocument(); const operation = document.paths["/internal/v1/usage-analysis"].get; assert.equal(operation["x-required-scope"], "requests:read"); const parameters = Object.fromEntries(operation.parameters.filter((item: Obj) => "name" in item).map((item: Obj) => [item.name, item]));
  assert.deepEqual(parameters.granularity.schema.enum, ["auto", "hour", "day"]); assert.deepEqual(parameters.protocol.schema.enum, ["openai", "anthropic", "openai-image", "generation"]); assert.deepEqual(parameters.status.schema.enum, ["success", "error"]); assert.deepEqual(parameters.upstream_account_id.schema.oneOf, [{ type: "string", format: "uuid" }, { type: "string", const: "unassigned" }]); assert.deepEqual(document.components.parameters.UpstreamAccountFilter.schema, { type: "string", format: "uuid" });
  const metrics = document.components.schemas.UsageAnalysisMetrics; for (const field of ["requests", "success", "failed", "cached_input_tokens", "cache_write_tokens", "generation_units", "costs"]) assert.ok(metrics.required.includes(field)); assert.equal(metrics.properties.costs.type, "array"); assert.equal(metrics.properties.costs.items.$ref, "#/components/schemas/UsageAnalysisCost"); const hour = document.components.schemas.UsageAnalysisHeatmapBucket.allOf[0].properties.hour_of_week; assert.deepEqual([hour.minimum, hour.maximum], [0, 167]);
});

test("OAuth reauthorization reuses the unified upstream resource", () => {
  const document = cloneDocument(); for (const [segment, schema] of [["cursor", "StartCursorOAuthRequest"], ["provider-adapter", "StartProviderAdapterOAuthRequest"], ["codex", "StartCodexOAuthRequest"]] as const) { const start = document.paths[`/internal/v1/oauth/${segment}/start`].post; const poll = document.paths[`/internal/v1/oauth/${segment}/poll`].post; assert.equal(start["x-required-scope"], "oauth:write"); assert.equal(poll["x-required-scope"], "oauth:write"); const target = document.components.schemas[schema].properties.upstream_account_id; assert.deepEqual([target.type, target.format], ["string", "uuid"]); assert.equal(poll.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/UpstreamProvider"); }
  for (const path of ["/internal/v1/oauth/subscription-bridge/start", "/internal/v1/oauth/subscription-bridge/poll", "/internal/v1/imports/cpa/subscription-accounts"]) assert.ok(!(path in document.paths)); for (const schema of ["StartSubscriptionBridgeRequest", "SubscriptionBridgeCredential"]) assert.ok(!(schema in document.components.schemas)); assert.equal(document.paths["/internal/v1/oauth/codex/start"].post.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/CodexDeviceLoginStart"); assert.equal(document.components.schemas.CodexDeviceLoginStart.properties.security_notice.const, "only_continue_if_you_started_this_login");
});

test("session archive quarantine is persistent global operator only", () => {
  const document = cloneDocument(); const base = "/internal/v1/imports/session-archive/quarantine"; const operations = [[document.paths[base].get, "imports:session_archive:quarantine:read"], [document.paths[`${base}/{quarantine_id}`].get, "imports:session_archive:quarantine:read"], [document.paths[`${base}/{quarantine_id}/resolutions`].post, "imports:session_archive:quarantine:resolve"]] as const;
  for (const [operation, scope] of operations) { assert.deepEqual(operation.security, [{ serviceBearer: [] }]); assert.equal(operation["x-required-scope"], scope); assert.equal(operation["x-global-service-only"], true); assert.equal(operation["x-persistent-service-only"], true); }
  const scopes: string[] = document.components.schemas.ServiceScope.enum; assert.ok(scopes.includes("imports:session_archive:quarantine:read")); assert.ok(scopes.includes("imports:session_archive:quarantine:resolve")); const required: string[] = document.components.schemas.ResolveSessionArchiveQuarantineRequest.required; assert.ok(required.includes("expected_record_digest") && required.includes("evidence_digest")); const properties = document.components.schemas.SessionArchiveQuarantineRecord.properties; for (const field of ["identity_claim_digest", "proof_digest", "request_object", "response_object"]) assert.ok(!(field in properties));
});

test("ContractFailure remains a distinct error type", () => assert.ok(new ContractFailure("x") instanceof Error));
