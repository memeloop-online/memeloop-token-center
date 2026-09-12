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
test("plugin operator data is a scoped typed-JSON proxy contract", () => {
  const document = cloneDocument(); const operation = document.paths["/internal/v1/plugins/{plugin_id}/data/{endpoint_id}"].get;
  assert.deepEqual(operation.security, [{ serviceBearer: [] }]);
  assert.equal(operation["x-required-scope"], "plugins:read");
  assert.equal(operation["x-manifest-required-scope"], true);
  assert.deepEqual(operation["x-outbound-contract"].methods, ["GET"]);
  assert.equal(operation["x-outbound-contract"].transport, "HTTPS-only-DNS-pinned-no-redirect");
  assert.equal(operation.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/PluginServiceDataResponse");
  const contribution = document.components.schemas.PluginOperatorUiContribution;
  assert.deepEqual(contribution.properties.slot.enum, ["operator.sidebar.tab", "operator.overview.card"]);
  assert.equal(contribution.properties.renderer.const, "typed_data_v1");
  assert.ok(document.components.schemas.PluginServiceDataEndpoint.properties.required_scope.enum.includes("metrics:read"));
});
test("Responses WebSocket negotiation remains an authenticated temporary fallback", () => {
  const document = cloneDocument(); const operation = document.paths["/v1/responses"].get; assert.deepEqual(operation.security, [{ clientBearer: [] }]); assert.deepEqual(operation.responses["426"], { $ref: "#/components/responses/ResponsesWebSocketUpgradeRequired" }); assert.equal(operation["x-transport-negotiation"]["native-websocket-planned"], true); validateProductContracts(document);
  delete operation["x-transport-negotiation"]["native-websocket-planned"]; assert.throws(() => validateProductContracts(document), /native WebSocket direction/u);
});
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

test("operator monitoring snapshot has explicit scope/window and bounded terminal drilldowns", () => {
  const document = cloneDocument(); const operation = document.paths["/internal/v1/monitoring-snapshot"].get;
  assert.equal(operation["x-required-scope"], "requests:read"); assert.deepEqual(operation.security, [{ serviceBearer: [] }]);
  const parameters = Object.fromEntries(operation.parameters.map((item: Obj) => [item.name, item]));
  assert.equal(parameters.scope.required, true); assert.deepEqual(parameters.scope.schema.enum, ["tenant", "global"]);
  assert.equal(parameters.from_created_at.required, true); assert.equal(parameters.to_created_at.required, true);
  const snapshot = document.components.schemas.OperatorMonitoringSnapshot;
  assert.equal(snapshot.properties.contract_version.const, "v1"); assert.equal(snapshot.properties.top_upstream_models.maxItems, 10);
  const health = document.components.schemas.MonitoringHealth;
  assert.deepEqual(health.properties.status.enum, ["healthy", "degraded", "unhealthy", "unknown"]);
  assert.equal(health.properties.version.const, "upstream_breaker_v1");
  assert.equal(document.components.schemas.MonitoringUpstreamModel.properties.terminal_outcomes.maxItems, 5);
  assert.equal(operation["x-query-plan"]["raw-request-records"], "forbidden");
});

test("upstream availability requires both scopes and an explicit tenant/window", () => {
  const document = cloneDocument(); const operation = document.paths["/internal/v1/upstream-availability"].get;
  assert.deepEqual(operation.security, [{ serviceBearer: [] }]);
  assert.equal(operation["x-required-scope"], "providers:read");
  assert.deepEqual(operation["x-required-scopes"], ["providers:read", "requests:read"]);
  const parameters = Object.fromEntries(operation.parameters.map((item: Obj) => [item.name, item]));
  assert.deepEqual(Object.keys(parameters), ["tenant_external_id", "from_created_at", "to_created_at"]);
  for (const parameter of Object.values(parameters) as Obj[]) assert.equal(parameter.required, true);
  for (const name of ["from_created_at", "to_created_at"]) assert.equal(parameters[name].schema.minimum, 0);
  assert.equal(operation.responses["200"].headers["Cache-Control"].schema.const, "no-store");
  assert.equal(operation.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/UpstreamAccountAvailabilityWindow");
  const window = document.components.schemas.UpstreamAccountAvailabilityWindow;
  assert.equal(window.properties.contract_version.const, "upstream_account_availability_v1");
  assert.deepEqual(window.properties.granularity.enum, ["hour", "day"]);
  assert.equal(window.properties.latency_is_approximate.const, true);
  assert.equal(window.properties.latency_method.const, "fixed_histogram_upper_bound_capped_60000ms");
  assert.equal(window.properties.accounts.maxItems, undefined);
  const account = document.components.schemas.UpstreamAccountAvailability;
  assert.deepEqual(account.required, ["upstream_account_id", "metrics", "terminal_outcomes"]);
  assert.equal(account.properties.metrics.$ref, "#/components/schemas/MonitoringMetrics");
  assert.equal(account.properties.terminal_outcomes.maxItems, 5);
  assert.equal(account.properties.terminal_outcomes.items.$ref, "#/components/schemas/MonitoringTerminalOutcome");
});

test("account settlements require both scopes and expose an immutable sequence cursor envelope", () => {
  const document = cloneDocument(); const operation = document.paths["/internal/v1/accounts/{account_id}/settlements"].get;
  assert.deepEqual(operation.security, [{ serviceBearer: [] }]);
  assert.equal(operation["x-required-scope"], "credits:read");
  assert.deepEqual(operation["x-required-scopes"], ["credits:read", "requests:read"]);
  assert.deepEqual(operation.parameters.map((parameter: Obj) => parameter.$ref), ["#/components/parameters/AccountId", "#/components/parameters/SettlementListLimit500", "#/components/parameters/AfterSettlementSequence", "#/components/parameters/AfterSettlementId", "#/components/parameters/SettlementRequestKind", "#/components/parameters/SettlementRequestId"]);
  assert.deepEqual(document.components.parameters.SettlementListLimit500.schema, { type: "integer", format: "int64", minimum: 1, maximum: 500, default: 100 });
  assert.deepEqual(document.components.parameters.AfterSettlementSequence.schema, { type: "integer", format: "int64", minimum: 1 });
  assert.deepEqual(document.components.parameters.AfterSettlementId.schema, { type: "string", format: "uuid" });
  assert.deepEqual(document.components.parameters.SettlementRequestKind.schema, { $ref: "#/components/schemas/AccountSettlementKind" });
  assert.deepEqual(document.components.parameters.SettlementRequestId.schema, { type: "string", format: "uuid" });
  assert.equal(operation.responses["200"].headers["Cache-Control"].schema.const, "no-store");
  assert.equal(operation.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/AccountSettlementPage");
  const page = document.components.schemas.AccountSettlementPage;
  assert.equal(page.additionalProperties, false); assert.deepEqual(page.required, ["items", "next_cursor"]); assert.equal(page.properties.items.maxItems, 500); assert.equal(page.properties.items.items.$ref, "#/components/schemas/AccountSettlement"); assert.deepEqual(page.properties.next_cursor.oneOf, [{ $ref: "#/components/schemas/AccountSettlementCursor" }, { type: "null" }]);
  const cursor = document.components.schemas.AccountSettlementCursor;
  assert.equal(cursor.additionalProperties, false); assert.deepEqual(cursor.required, ["after_sequence", "after_id"]); assert.deepEqual(cursor.properties.after_sequence, { type: "integer", format: "int64" }); assert.deepEqual(cursor.properties.after_id, { type: "string", format: "uuid" });
  const item = document.components.schemas.AccountSettlement;
  assert.equal(item.additionalProperties, false); assert.deepEqual(item.required, ["settlement_id", "settlement_sequence", "request_id", "kind", "account_id", "key_id", "model", "cost", "currency", "settled_at", "completed_at", "input_tokens", "cached_input_tokens", "cache_write_tokens", "output_tokens"]); assert.deepEqual(document.components.schemas.AccountSettlementKind.enum, ["text", "generation"]); assert.deepEqual(item.properties.settlement_sequence, { type: "integer", format: "int64" }); assert.equal(item.properties.cost.$ref, "#/components/schemas/NonNegativeMoney"); for (const name of ["input_tokens", "cached_input_tokens", "cache_write_tokens", "output_tokens"]) assert.deepEqual(item.properties[name], { type: ["integer", "null"], format: "int64" });
});

test("OAuth reauthorization reuses the unified upstream resource", () => {
  const document = cloneDocument(); for (const [segment, schema] of [["cursor", "StartCursorOAuthRequest"], ["provider-adapter", "StartProviderAdapterOAuthRequest"], ["codex", "StartCodexOAuthRequest"]] as const) { const start = document.paths[`/internal/v1/oauth/${segment}/start`].post; const poll = document.paths[`/internal/v1/oauth/${segment}/poll`].post; assert.equal(start["x-required-scope"], "oauth:write"); assert.equal(poll["x-required-scope"], "oauth:write"); const target = document.components.schemas[schema].properties.upstream_account_id; assert.deepEqual([target.type, target.format], ["string", "uuid"]); assert.equal(poll.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/UpstreamProvider"); }
  for (const path of ["/internal/v1/oauth/subscription-bridge/start", "/internal/v1/oauth/subscription-bridge/poll", "/internal/v1/imports/cpa/subscription-accounts"]) assert.ok(!(path in document.paths)); for (const schema of ["StartSubscriptionBridgeRequest", "SubscriptionBridgeCredential"]) assert.ok(!(schema in document.components.schemas)); assert.equal(document.paths["/internal/v1/oauth/codex/start"].post.responses["200"].content["application/json"].schema.$ref, "#/components/schemas/CodexDeviceLoginStart"); assert.equal(document.components.schemas.CodexDeviceLoginStart.properties.security_notice.const, "only_continue_if_you_started_this_login");
});

test("native Kimi cohort import advertises the atomic v2 fail-closed contract", () => {
  const document = cloneDocument(); const base = "/internal/v1/native-oauth-imports"; const capabilities = document.paths[`${base}/capabilities`].get; const apply = document.paths[`${base}/kimi-cohort`].post;
  for (const operation of [capabilities, apply]) { assert.deepEqual(operation.security, [{ serviceBearer: [] }]); assert.equal(operation["x-required-scope"], "upstreams:import:write"); assert.equal(operation["x-global-service-only"], true); }
  const capabilitySchema = document.components.schemas.NativeOAuthImportCapabilities; assert.equal(capabilitySchema.properties.contract_version.const, 2); assert.equal(capabilitySchema.properties.source_identity_contract.const, "operator-hmac-sha256-v1"); assert.equal(capabilitySchema.properties.account_name_policies.properties.kimi.const, "neutral-server-keyed-source-suffix-v1"); assert.deepEqual(capabilitySchema.properties.atomic_cohort_contracts.prefixItems, [{ const: "atomic_kimi_cohort_v2" }]); assert.equal(capabilitySchema.properties.credential_envelope_contract.const, "chacha20poly1305-hkdf-sha256-v2-aad-v1");
  const request = document.components.schemas.NativeKimiOAuthCohortRequest; assert.deepEqual([request.properties.accounts.minItems, request.properties.accounts.maxItems], [2, 2]); assert.equal(request.additionalProperties, false); assert.equal(document.components.schemas.NativeKimiOAuthCohortAccount.additionalProperties, false); assert.ok(document.components.schemas.ServiceScope.enum.includes("upstreams:import:write"));
  const upstream = document.components.schemas.UpstreamProvider; for (const field of ["import_source_identity_hash", "import_source_document_sha256"]) assert.ok(upstream.required.includes(field));
});

test("session archive quarantine is persistent global operator only", () => {
  const document = cloneDocument(); const base = "/internal/v1/imports/session-archive/quarantine"; const operations = [[document.paths[base].get, "imports:session_archive:quarantine:read"], [document.paths[`${base}/{quarantine_id}`].get, "imports:session_archive:quarantine:read"], [document.paths[`${base}/{quarantine_id}/resolutions`].post, "imports:session_archive:quarantine:resolve"]] as const;
  for (const [operation, scope] of operations) { assert.deepEqual(operation.security, [{ serviceBearer: [] }]); assert.equal(operation["x-required-scope"], scope); assert.equal(operation["x-global-service-only"], true); assert.equal(operation["x-persistent-service-only"], true); }
  const scopes: string[] = document.components.schemas.ServiceScope.enum; assert.ok(scopes.includes("imports:session_archive:quarantine:read")); assert.ok(scopes.includes("imports:session_archive:quarantine:resolve")); const required: string[] = document.components.schemas.ResolveSessionArchiveQuarantineRequest.required; assert.ok(required.includes("expected_record_digest") && required.includes("evidence_digest")); const properties = document.components.schemas.SessionArchiveQuarantineRecord.properties; for (const field of ["identity_claim_digest", "proof_digest", "request_object", "response_object"]) assert.ok(!(field in properties));
});

test("ContractFailure remains a distinct error type", () => assert.ok(new ContractFailure("x") instanceof Error));
