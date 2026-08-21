#!/usr/bin/env python3
"""Focused regression tests for the Rust route extractor."""

from __future__ import annotations

import importlib.util
import copy
import pathlib
import tempfile
import unittest


SCRIPT = pathlib.Path(__file__).with_name("check-openapi-contract.py")
SPEC = importlib.util.spec_from_file_location("check_openapi_contract", SCRIPT)
assert SPEC and SPEC.loader
CONTRACT = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CONTRACT)


def source_with(
    control_extra: str = "", gateway_extra: str = "", common_extra: str = ""
) -> str:
    return f"""
fn router_for_role(state: AppState, role: RuntimeRole) -> Router {{
    let mut application = Router::new()
        .route("/livez", get(liveness));
    // .route("/ghost", get(comment_must_not_be_parsed));
    if matches!(role, RuntimeRole::Control | RuntimeRole::All) {{
        application = application.route("/metrics", get(metrics));
    }}
    application = application.merge(control_router(state.clone()));{common_extra}
    application = application.merge(gateway_router(state));
    application
}}
fn control_router(state: AppState) -> Router<AppState> {{
    Router::new()
        .route("/internal/v1/example", get(example)){control_extra}
}}
fn gateway_router(state: AppState) -> Router<AppState> {{
    Router::new()
        .route("/v1/example", get(example)){gateway_extra}
}}
"""


class RouteExtractorTests(unittest.TestCase):
    def test_source_directory_is_combined_in_stable_relative_path_order(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            (root / "nested").mkdir()
            (root / "z.rs").write_text("fn z() {}\n", encoding="utf-8")
            (root / "nested" / "a.rs").write_text("fn a() {}\n", encoding="utf-8")

            source = CONTRACT.read_rust_source(root)

        self.assertLess(source.index("nested/a.rs"), source.index("z.rs"))

    def test_comments_are_ignored_and_control_guard_is_classified(self) -> None:
        routes = CONTRACT.source_routes(source_with())
        self.assertNotIn("/ghost", {route["path"] for route in routes})
        self.assertEqual(
            "control",
            next(route["source_role"] for route in routes if route["path"] == "/metrics"),
        )
        self.assertEqual(4, len(routes))

    def test_route_service_fails_closed(self) -> None:
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "route_service"):
            CONTRACT.source_routes(
                source_with(gateway_extra='.route_service("/opaque", service)')
            )

    def test_handler_comments_do_not_add_methods(self) -> None:
        routes = CONTRACT.source_routes(
            source_with(
                gateway_extra='.route("/v1/comment", post(handler) /* get(fake) */)'
            )
        )
        comment = [route for route in routes if route["path"] == "/v1/comment"]
        self.assertEqual(["post"], [route["method"] for route in comment])

    def test_fallback_fails_closed(self) -> None:
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "fallback"):
            CONTRACT.source_routes(source_with(gateway_extra=".fallback(handler)"))

    def test_lifetime_apostrophe_is_not_parsed_as_a_character_literal(self) -> None:
        routes = CONTRACT.source_routes(source_with(common_extra=".layer(foo::<'static>())"))
        self.assertEqual(4, len(routes))

    def test_unknown_merged_router_fails_closed(self) -> None:
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "unparsed Router"):
            CONTRACT.source_routes(source_with(control_extra=".merge(helper_router())"))


class ProductContractTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        repository = SCRIPT.resolve().parents[2]
        cls.document = CONTRACT.yaml.safe_load(
            (repository / "openapi/openapi.yaml").read_text(encoding="utf-8")
        )

    def test_asset_and_image_semantics_are_complete(self) -> None:
        CONTRACT.validate_product_contracts(self.document)

    def test_self_limit_snapshot_security_regression_fails_closed(self) -> None:
        document = copy.deepcopy(self.document)
        document["paths"]["/self/v1/key/limits"]["get"]["security"] = [
            {"serviceBearer": []}
        ]
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "credential security"):
            CONTRACT.validate_product_contracts(document)

    def test_usage_limit_reason_regression_fails_closed(self) -> None:
        document = copy.deepcopy(self.document)
        document["components"]["schemas"]["UsageLimitErrorResponse"]["properties"]["error"]["required"].remove("retryable")
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "reason/retryable"):
            CONTRACT.validate_product_contracts(document)

    def test_asset_range_response_regression_fails_closed(self) -> None:
        document = copy.deepcopy(self.document)
        del document["paths"][
            "/self/v1/requests/{request_id}/assets/{asset_id}"
        ]["get"]["responses"]["416"]
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "must retain responses"):
            CONTRACT.validate_product_contracts(document)

    def test_synchronous_image_namespace_regression_fails_closed(self) -> None:
        document = copy.deepcopy(self.document)
        document["paths"]["/v1/images/generations"]["post"][
            "x-idempotency-contract"
        ]["namespace"] = "raw-client-secret"
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "idempotency contract"):
            CONTRACT.validate_product_contracts(document)

    def test_cloud_subscription_policy_rollback_regression_fails_closed(self) -> None:
        document = copy.deepcopy(self.document)
        document["paths"][
            "/internal/v1/integrations/memeloop-cloud/subscription"
        ]["put"]["x-idempotency-contract"]["policy-update"] = "unversioned"
        with self.assertRaisesRegex(
            CONTRACT.ContractFailure, "Cloud ordered idempotency contract"
        ):
            CONTRACT.validate_product_contracts(document)

    def test_cloud_subscription_hmac_regression_fails_closed(self) -> None:
        document = copy.deepcopy(self.document)
        document["paths"][
            "/internal/v1/integrations/memeloop-cloud/subscription"
        ]["put"]["security"] = [{"serviceBearer": []}]
        with self.assertRaisesRegex(CONTRACT.ContractFailure, "lost HMAC security"):
            CONTRACT.validate_product_contracts(document)

    def test_usage_analysis_contract_is_currency_safe_and_canonical(self) -> None:
        operation = self.document["paths"]["/internal/v1/usage-analysis"]["get"]
        self.assertEqual("requests:read", operation["x-required-scope"])
        parameters = {
            parameter["name"]: parameter
            for parameter in operation["parameters"]
            if "name" in parameter
        }
        self.assertEqual(
            ["auto", "hour", "day"], parameters["granularity"]["schema"]["enum"]
        )
        self.assertEqual(
            ["openai", "anthropic", "openai-image", "generation"],
            parameters["protocol"]["schema"]["enum"],
        )
        self.assertEqual(
            ["success", "error"], parameters["status"]["schema"]["enum"]
        )
        self.assertEqual(
            [
                {"type": "string", "format": "uuid"},
                {"type": "string", "const": "unassigned"},
            ],
            parameters["upstream_account_id"]["schema"]["oneOf"],
        )
        self.assertEqual(
            {"type": "string", "format": "uuid"},
            self.document["components"]["parameters"]["UpstreamAccountFilter"][
                "schema"
            ],
        )
        metrics = self.document["components"]["schemas"]["UsageAnalysisMetrics"]
        self.assertTrue(
            {
                "requests",
                "success",
                "failed",
                "cached_input_tokens",
                "cache_write_tokens",
                "generation_units",
                "costs",
            }.issubset(metrics["required"])
        )
        costs = metrics["properties"]["costs"]
        self.assertEqual("array", costs["type"])
        self.assertEqual(
            "#/components/schemas/UsageAnalysisCost", costs["items"]["$ref"]
        )
        heatmap = self.document["components"]["schemas"][
            "UsageAnalysisHeatmapBucket"
        ]
        hour = heatmap["allOf"][0]["properties"]["hour_of_week"]
        self.assertEqual((0, 167), (hour["minimum"], hour["maximum"]))

    def test_oauth_reauthorization_reuses_the_unified_upstream_resource(self) -> None:
        cases = [
            ("cursor", "StartCursorOAuthRequest"),
            ("provider-adapter", "StartProviderAdapterOAuthRequest"),
            ("codex", "StartCodexOAuthRequest"),
        ]
        for path_segment, request_schema in cases:
            start = self.document["paths"][
                f"/internal/v1/oauth/{path_segment}/start"
            ]["post"]
            poll = self.document["paths"][
                f"/internal/v1/oauth/{path_segment}/poll"
            ]["post"]
            self.assertEqual("oauth:write", start["x-required-scope"])
            self.assertEqual("oauth:write", poll["x-required-scope"])
            target = self.document["components"]["schemas"][request_schema][
                "properties"
            ]["upstream_account_id"]
            self.assertEqual(("string", "uuid"), (target["type"], target["format"]))
            self.assertEqual(
                "#/components/schemas/UpstreamProvider",
                poll["responses"]["200"]["content"]["application/json"][
                    "schema"
                ]["$ref"],
            )
        paths = self.document["paths"]
        self.assertNotIn("/internal/v1/oauth/subscription-bridge/start", paths)
        self.assertNotIn("/internal/v1/oauth/subscription-bridge/poll", paths)
        self.assertNotIn("/internal/v1/imports/cpa/subscription-accounts", paths)
        schemas = self.document["components"]["schemas"]
        self.assertNotIn("StartSubscriptionBridgeRequest", schemas)
        self.assertNotIn("SubscriptionBridgeCredential", schemas)
        codex_start = paths["/internal/v1/oauth/codex/start"]["post"]
        self.assertEqual(
            "#/components/schemas/CodexDeviceLoginStart",
            codex_start["responses"]["200"]["content"]["application/json"][
                "schema"
            ]["$ref"],
        )
        self.assertEqual(
            "only_continue_if_you_started_this_login",
            schemas["CodexDeviceLoginStart"]["properties"]["security_notice"][
                "const"
            ],
        )

    def test_session_archive_quarantine_is_persistent_global_operator_only(self) -> None:
        base = "/internal/v1/imports/session-archive/quarantine"
        operations = [
            (self.document["paths"][base]["get"], "imports:session_archive:quarantine:read"),
            (
                self.document["paths"][f"{base}/{{quarantine_id}}"]["get"],
                "imports:session_archive:quarantine:read",
            ),
            (
                self.document["paths"][f"{base}/{{quarantine_id}}/resolutions"]["post"],
                "imports:session_archive:quarantine:resolve",
            ),
        ]
        for operation, scope in operations:
            self.assertEqual([{"serviceBearer": []}], operation["security"])
            self.assertEqual(scope, operation["x-required-scope"])
            self.assertIs(operation["x-global-service-only"], True)
            self.assertIs(operation["x-persistent-service-only"], True)

        scopes = self.document["components"]["schemas"]["ServiceScope"]["enum"]
        self.assertIn("imports:session_archive:quarantine:read", scopes)
        self.assertIn("imports:session_archive:quarantine:resolve", scopes)
        request = self.document["components"]["schemas"][
            "ResolveSessionArchiveQuarantineRequest"
        ]
        self.assertTrue(
            {"expected_record_digest", "evidence_digest"}.issubset(request["required"])
        )
        record_properties = self.document["components"]["schemas"][
            "SessionArchiveQuarantineRecord"
        ]["properties"]
        for secret_internal_field in (
            "identity_claim_digest",
            "proof_digest",
            "request_object",
            "response_object",
        ):
            self.assertNotIn(secret_internal_field, record_properties)


if __name__ == "__main__":
    unittest.main()
