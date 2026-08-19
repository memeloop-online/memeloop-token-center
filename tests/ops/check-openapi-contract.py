#!/usr/bin/env python3
"""Fail when Axum routes, OpenAPI operations, or role boundaries diverge."""

from __future__ import annotations

import argparse
import json
import pathlib
import re
import sys
from collections.abc import Iterator
from typing import Any

try:
    import yaml
    from openapi_spec_validator import validate_spec
    from openapi_spec_validator.validation.exceptions import OpenAPIValidationError
except ImportError as error:  # pragma: no cover - exercised by operator setup
    raise SystemExit(
        "missing validation dependencies; install requirements-openapi.txt"
    ) from error


HTTP_METHODS = {"delete", "get", "head", "options", "patch", "post", "put", "trace"}
ROUTE_METHOD = re.compile(
    r"(?<![A-Za-z0-9_])(delete|get|head|options|patch|post|put|trace)\s*\("
)
EXPECTED_RUNTIME_ROLES = {
    "common": ["gateway", "control", "worker", "all"],
    "control": ["control", "all"],
    "gateway": ["gateway", "all"],
}


class ContractFailure(RuntimeError):
    """The checked release contract is incomplete or inconsistent."""


def rust_char_literal_at(source: str, index: int) -> bool:
    """Distinguish a simple Rust character literal from a lifetime apostrophe."""
    if source[index] != "'" or index + 2 >= len(source):
        return False
    if source[index + 1] == "\\":
        closing = source.find("'", index + 2)
        return 0 < closing - index <= 12
    return source[index + 2] == "'"


def balanced_slice(source: str, start: int, opening: str, closing: str) -> tuple[str, int]:
    if source[start] != opening:
        raise ContractFailure(f"expected {opening!r} at byte {start}")
    depth = 0
    quote: str | None = None
    escaped = False
    line_comment = False
    block_comment = 0
    index = start
    while index < len(source):
        char = source[index]
        following = source[index + 1] if index + 1 < len(source) else ""
        if line_comment:
            if char == "\n":
                line_comment = False
            index += 1
            continue
        if block_comment:
            if char == "/" and following == "*":
                block_comment += 1
                index += 2
                continue
            if char == "*" and following == "/":
                block_comment -= 1
                index += 2
                continue
            index += 1
            continue
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            index += 1
            continue
        if char == "/" and following == "/":
            line_comment = True
            index += 2
            continue
        if char == "/" and following == "*":
            block_comment = 1
            index += 2
            continue
        if char == '"' or (char == "'" and rust_char_literal_at(source, index)):
            quote = char
            index += 1
            continue
        if char == opening:
            depth += 1
        elif char == closing:
            depth -= 1
            if depth == 0:
                return source[start + 1 : index], index + 1
        index += 1
    raise ContractFailure(f"unbalanced {opening}{closing} beginning at byte {start}")


def function_body(source: str, function_name: str) -> str:
    match = re.search(rf"\bfn\s+{re.escape(function_name)}\s*\(", source)
    if not match:
        raise ContractFailure(f"Rust function {function_name} was not found")
    brace = source.find("{", match.end())
    if brace < 0:
        raise ContractFailure(f"Rust function {function_name} has no body")
    body, _end = balanced_slice(source, brace, "{", "}")
    return body


def code_mask(source: str) -> list[bool]:
    """Mark bytes that are Rust code rather than strings or comments."""
    mask = [False] * len(source)
    quote: str | None = None
    escaped = False
    line_comment = False
    block_comment = 0
    index = 0
    while index < len(source):
        char = source[index]
        following = source[index + 1] if index + 1 < len(source) else ""
        if line_comment:
            if char == "\n":
                line_comment = False
                mask[index] = True
            index += 1
            continue
        if block_comment:
            if char == "/" and following == "*":
                block_comment += 1
                index += 2
                continue
            if char == "*" and following == "/":
                block_comment -= 1
                index += 2
                continue
            index += 1
            continue
        if quote:
            if escaped:
                escaped = False
            elif char == "\\":
                escaped = True
            elif char == quote:
                quote = None
            index += 1
            continue
        if char == "/" and following == "/":
            line_comment = True
            index += 2
            continue
        if char == "/" and following == "*":
            block_comment = 1
            index += 2
            continue
        if char == '"' or (char == "'" and rust_char_literal_at(source, index)):
            quote = char
            index += 1
            continue
        mask[index] = True
        index += 1
    return mask


def fail_on_unparsed_router_composition(body: str, function_name: str, mask: list[bool]) -> None:
    for unsupported in (
        ".fallback",
        ".fallback_service",
        ".nest",
        ".nest_service",
        ".route_service",
    ):
        cursor = 0
        while True:
            marker = body.find(unsupported, cursor)
            if marker < 0:
                break
            if mask[marker]:
                raise ContractFailure(
                    f"{function_name} uses unsupported Router composition {unsupported}; "
                    "extend the contract extractor before adding it"
                )
            cursor = marker + len(unsupported)
    cursor = 0
    while True:
        marker = body.find(".merge", cursor)
        if marker < 0:
            return
        if not mask[marker]:
            cursor = marker + len(".merge")
            continue
        opening = marker + len(".merge")
        while opening < len(body) and body[opening].isspace():
            opening += 1
        if opening >= len(body) or body[opening] != "(":
            cursor = opening
            continue
        arguments, end = balanced_slice(body, opening, "(", ")")
        expression = arguments.strip()
        if not (
            expression == "authenticated"
            or expression.startswith("control_router(")
            or expression.startswith("gateway_router(")
        ):
            raise ContractFailure(
                f"{function_name} merges an unparsed Router expression: {expression[:120]}"
            )
        cursor = end


def control_guard_ranges(body: str, function_name: str) -> list[tuple[int, int]]:
    if function_name != "router_for_role":
        return []
    guard = re.compile(
        r"if\s+matches!\(\s*role\s*,\s*RuntimeRole::Control\s*\|\s*RuntimeRole::All\s*\)\s*\{"
    )
    ranges = []
    for match in guard.finditer(body):
        opening = body.rfind("{", match.start(), match.end())
        _contents, end = balanced_slice(body, opening, "{", "}")
        ranges.append((opening, end))
    if len(ranges) != 1:
        raise ContractFailure(
            "router_for_role must retain one explicit Control|All guard for private system routes"
        )
    return ranges


def source_routes(source: str) -> list[dict[str, str]]:
    routes: list[dict[str, str]] = []
    for role, function_name in (
        ("common", "router_for_role"),
        ("control", "control_router"),
        ("gateway", "gateway_router"),
    ):
        body = function_body(source, function_name)
        mask = code_mask(body)
        fail_on_unparsed_router_composition(body, function_name, mask)
        guarded_control = control_guard_ranges(body, function_name)
        cursor = 0
        while True:
            marker = body.find(".route", cursor)
            if marker < 0:
                break
            if not mask[marker]:
                cursor = marker + len(".route")
                continue
            after_marker = marker + len(".route")
            while after_marker < len(body) and body[after_marker].isspace():
                after_marker += 1
            if after_marker >= len(body) or body[after_marker] != "(":
                cursor = after_marker
                continue
            arguments, end = balanced_slice(body, after_marker, "(", ")")
            path_match = re.match(r'\s*"([^"\\]+)"\s*,', arguments)
            if not path_match:
                raise ContractFailure(
                    f"{function_name} contains a .route call without a literal path"
                )
            path = path_match.group(1)
            effective_role = (
                "control"
                if role == "common"
                and any(start < marker < end for start, end in guarded_control)
                else role
            )
            handler_expression = arguments[path_match.end() :]
            handler_mask = code_mask(handler_expression)
            methods = sorted(
                {
                    match.group(1)
                    for match in ROUTE_METHOD.finditer(handler_expression)
                    if handler_mask[match.start()]
                }
            )
            if not methods:
                raise ContractFailure(f"source route {path} has no recognized HTTP method")
            routes.extend(
                {"method": method, "path": path, "source_role": effective_role}
                for method in methods
            )
            cursor = end
    duplicates = duplicate_keys(routes, ("method", "path"))
    if duplicates:
        raise ContractFailure(f"duplicate source method/path routes: {duplicates}")
    return sorted(routes, key=lambda route: (route["path"], route["method"]))


def duplicate_keys(items: list[dict[str, Any]], fields: tuple[str, ...]) -> list[str]:
    seen: set[tuple[Any, ...]] = set()
    duplicates: set[tuple[Any, ...]] = set()
    for item in items:
        key = tuple(item[field] for field in fields)
        if key in seen:
            duplicates.add(key)
        seen.add(key)
    return [" ".join(str(part) for part in key) for key in sorted(duplicates)]


def resolve_pointer(document: object, pointer: str) -> object:
    if not pointer.startswith("#/"):
        raise ContractFailure(f"only local OpenAPI references are permitted: {pointer}")
    current = document
    for raw_part in pointer[2:].split("/"):
        part = raw_part.replace("~1", "/").replace("~0", "~")
        if not isinstance(current, dict) or part not in current:
            raise ContractFailure(f"unresolvable OpenAPI reference: {pointer}")
        current = current[part]
    return current


def references(value: object) -> Iterator[str]:
    if isinstance(value, dict):
        ref = value.get("$ref")
        if isinstance(ref, str):
            yield ref
        for child in value.values():
            yield from references(child)
    elif isinstance(value, list):
        for child in value:
            yield from references(child)


def openapi_routes(document: dict[str, Any]) -> list[dict[str, Any]]:
    paths = document.get("paths")
    if not isinstance(paths, dict):
        raise ContractFailure("OpenAPI paths must be an object")
    routes: list[dict[str, Any]] = []
    for path, path_item in paths.items():
        if not isinstance(path, str) or not isinstance(path_item, dict):
            raise ContractFailure("OpenAPI path items must be objects")
        for method, operation in path_item.items():
            method = method.lower()
            if method not in HTTP_METHODS:
                continue
            if not isinstance(operation, dict):
                raise ContractFailure(f"{method.upper()} {path} operation must be an object")
            operation_id = operation.get("operationId")
            if not isinstance(operation_id, str) or not operation_id:
                raise ContractFailure(f"{method.upper()} {path} has no operationId")
            responses = operation.get("responses")
            if not isinstance(responses, dict) or not responses:
                raise ContractFailure(f"{operation_id} has no response contract")
            schema_refs = sorted(set(references(operation)))
            for ref in schema_refs:
                resolve_pointer(document, ref)
            routes.append(
                {
                    "method": method,
                    "path": path,
                    "operation_id": operation_id,
                    "schema_refs": schema_refs,
                }
            )
    duplicates = duplicate_keys(routes, ("method", "path"))
    duplicate_ids = duplicate_keys(routes, ("operation_id",))
    if duplicates or duplicate_ids:
        raise ContractFailure(
            f"duplicate OpenAPI routes={duplicates}, operationIds={duplicate_ids}"
        )
    return sorted(routes, key=lambda route: (route["path"], route["method"]))


def operation_at(document: dict[str, Any], method: str, path: str) -> dict[str, Any]:
    operation = document.get("paths", {}).get(path, {}).get(method)
    if not isinstance(operation, dict):
        raise ContractFailure(f"required OpenAPI operation is missing: {method.upper()} {path}")
    return operation


def parameter_references(operation: dict[str, Any]) -> set[str]:
    parameters = operation.get("parameters", [])
    if not isinstance(parameters, list):
        raise ContractFailure("OpenAPI operation parameters must be an array")
    return {
        parameter["$ref"]
        for parameter in parameters
        if isinstance(parameter, dict) and isinstance(parameter.get("$ref"), str)
    }


def validate_product_contracts(document: dict[str, Any]) -> None:
    """Pin security-critical credential, asset, and synchronous-image semantics in CI."""
    credential_operations = (
        ("patch", "/internal/v1/keys/{key_id}/alias", [{"serviceBearer": []}], "keys:write"),
        ("get", "/internal/v1/keys/{key_id}/limits", [{"serviceBearer": []}], "keys:read"),
        ("get", "/self/v1/key/limits", [{"clientBearer": []}], None),
    )
    for method, path, security, scope in credential_operations:
        operation = operation_at(document, method, path)
        if operation.get("security") != security:
            raise ContractFailure(f"{method.upper()} {path} credential security changed")
        if operation.get("x-required-scope") != scope:
            raise ContractFailure(f"{method.upper()} {path} credential scope changed")
        response = operation.get("responses", {}).get("200", {})
        schema = response.get("content", {}).get("application/json", {}).get("schema", {})
        if path.endswith("/limits") and schema.get("$ref") != "#/components/schemas/ClientCredentialLimitSnapshot":
            raise ContractFailure(f"{method.upper()} {path} limit snapshot schema changed")

    legacy = operation_at(document, "post", "/internal/v1/keys/{key_id}/legacy-credentials")
    if "409" not in legacy.get("responses", {}):
        raise ContractFailure("legacy credential one-to-one conflicts must retain HTTP 409")
    legacy_schema = legacy["requestBody"]["content"]["application/json"]["schema"]["properties"]
    if legacy_schema.get("credential", {}).get("minLength") != 16 or legacy_schema.get("credential", {}).get("maxLength") != 512:
        raise ContractFailure("legacy credential runtime length contract changed")
    source_hash = legacy_schema.get("source_hash", {})
    if source_hash.get("minLength") != 64 or source_hash.get("maxLength") != 64:
        raise ContractFailure("legacy credential source hash must remain an exact digest")

    policy = document.get("components", {}).get("schemas", {}).get("CredentialPolicy", {})
    policy_properties = policy.get("properties", {})
    if policy_properties.get("tokens_per_minute", {}).get("maximum") != 9_007_199_254_740_991:
        raise ContractFailure("credential TPM must remain JSON-safe")
    if policy_properties.get("allowed_models", {}).get("maxItems") != 500:
        raise ContractFailure("credential model allowlist bound changed")

    usage_rejected = document.get("components", {}).get("responses", {}).get("UsageRejected", {})
    if "Retry-After" not in usage_rejected.get("headers", {}):
        raise ContractFailure("usage rejection lost Retry-After contract")
    usage_schema = document.get("components", {}).get("schemas", {}).get("UsageLimitErrorResponse", {})
    error_schema = usage_schema.get("properties", {}).get("error", {})
    if not {"code", "message", "reason", "retryable"}.issubset(set(error_schema.get("required", []))):
        raise ContractFailure("usage rejection lost fixed reason/retryable fields")
    expected_reasons = {
        "balance_exhausted", "daily_budget_exhausted", "weekly_budget_exhausted",
        "lifetime_budget_exhausted", "rpm_exhausted", "tpm_exhausted",
        "concurrency_exhausted",
    }
    if set(error_schema.get("properties", {}).get("reason", {}).get("enum", [])) != expected_reasons:
        raise ContractFailure("usage rejection reason enum changed")

    asset_contracts = (
        (
            "/internal/v1/requests/{request_id}/assets/{asset_id}",
            [{"serviceBearer": []}],
            "requests:read",
            {
                "#/components/parameters/RequestIdPath",
                "#/components/parameters/AssetIdPath",
                "#/components/parameters/TenantFilter",
                "#/components/parameters/OptionalByteRange",
            },
            {"identity": "service-credential", "tenant-isolation-failure": 404},
            {"200", "206", "401", "403", "404", "416", "500"},
        ),
        (
            "/self/v1/requests/{request_id}/assets/{asset_id}",
            [{"clientBearer": []}],
            None,
            {
                "#/components/parameters/RequestIdPath",
                "#/components/parameters/AssetIdPath",
                "#/components/parameters/OptionalByteRange",
            },
            {"identity": "stable-key-id", "ownership-failure": 404},
            {"200", "206", "401", "404", "416", "500"},
        ),
    )
    for path, security, scope, parameters, authorization, responses in asset_contracts:
        operation = operation_at(document, "get", path)
        if operation.get("security") != security:
            raise ContractFailure(f"GET {path} must retain {security} security")
        if operation.get("x-required-scope") != scope:
            raise ContractFailure(f"GET {path} must retain scope {scope!r}")
        if parameter_references(operation) != parameters:
            raise ContractFailure(f"GET {path} asset/tenant/range parameters changed")
        response_codes = set(operation.get("responses", {}))
        if not responses.issubset(response_codes):
            raise ContractFailure(f"GET {path} must retain responses {sorted(responses)}")
        for status, reference in {
            "200": "#/components/responses/GenerationAssetFull",
            "206": "#/components/responses/GenerationAssetPartial",
            "404": "#/components/responses/NotFound",
            "416": "#/components/responses/RangeNotSatisfiable",
            "500": "#/components/responses/InternalError",
        }.items():
            if operation["responses"].get(status) != {"$ref": reference}:
                raise ContractFailure(f"GET {path} response {status} contract changed")
        actual_authorization = operation.get("x-authorization-contract")
        if not isinstance(actual_authorization, dict) or any(
            actual_authorization.get(key) != value for key, value in authorization.items()
        ):
            raise ContractFailure(f"GET {path} ownership/tenant isolation contract changed")
        if actual_authorization.get("object-binding") != "request-id-and-asset-id":
            raise ContractFailure(f"GET {path} must bind both opaque identifiers")
        if actual_authorization.get("archive-locator-exposed") is not False:
            raise ContractFailure(f"GET {path} must not expose archive locators")

    image = operation_at(document, "post", "/v1/images/generations")
    if image.get("security") != [{"clientBearer": []}]:
        raise ContractFailure("POST /v1/images/generations must retain client security")
    if "#/components/parameters/OptionalIdempotencyKey" not in parameter_references(image):
        raise ContractFailure("POST /v1/images/generations lost optional Idempotency-Key")
    required_responses = {"200", "202", "400", "401", "403", "409", "424", "429", "502"}
    if not required_responses.issubset(set(image.get("responses", {}))):
        raise ContractFailure("POST /v1/images/generations response contract regressed")
    idempotency = image.get("x-idempotency-contract")
    required_idempotency = {
        "namespace": "stable-key-id",
        "fingerprint": "post-policy-canonical-model-and-request",
        "completed-replay": "exact-status-body-and-request-id",
        "different-payload-status": 400,
        "in-progress-status": 409,
        "expired-pending": "atomically-reclaimed",
        "persisted-url-results": "mtc-request-asset-reference-only",
    }
    if not isinstance(idempotency, dict) or any(
        idempotency.get(key) != value for key, value in required_idempotency.items()
    ):
        raise ContractFailure("POST /v1/images/generations idempotency contract changed")
    required_effects = {
        "no-request-record",
        "no-upstream-request",
        "no-quota-reservation",
        "no-usage-settlement",
        "no-key-rate-limit-window-consumption",
        "no-image-execution-permit",
    }
    if set(idempotency.get("completed-replay-effects", [])) != required_effects:
        raise ContractFailure("completed synchronous image replay acquired side effects")
    if set(idempotency.get("replay-preconditions", [])) != {
        "authentication",
        "traffic-policy-evaluation",
    }:
        raise ContractFailure("synchronous image replay preconditions changed")
    if set(idempotency.get("fresh-execution-only", [])) != {
        "route-resolution",
        "price-lookup",
        "outbound-client-validation",
    }:
        raise ContractFailure("completed image replay regained mutable route/price dependencies")

    cloud = operation_at(
        document,
        "put",
        "/internal/v1/integrations/memeloop-cloud/subscription",
    )
    if cloud.get("security") != [{"memeloopCloudHmac": []}]:
        raise ContractFailure("MemeLoop Cloud subscription sync lost HMAC security")
    cloud_parameters = {
        parameter.get("name"): parameter
        for parameter in cloud.get("parameters", [])
        if isinstance(parameter, dict) and "name" in parameter
    }
    if parameter_references(cloud) != {
        "#/components/parameters/RequiredIdempotencyKey"
    } or set(cloud_parameters) != {
        "X-MTC-Webhook-Timestamp",
        "X-MTC-Webhook-Signature",
    }:
        raise ContractFailure("MemeLoop Cloud signature/idempotency headers changed")
    if not {"200", "201", "400", "401", "403", "404", "409"}.issubset(
        set(cloud.get("responses", {}))
    ):
        raise ContractFailure("MemeLoop Cloud lifecycle response contract regressed")
    signature = cloud.get("x-signature-contract")
    if signature != {
        "algorithm": "HMAC-SHA-256",
        "encoding": "base64url-no-padding",
        "envelope": "ascii-timestamp-dot-exact-body",
        "tolerance-seconds": 300,
        "disabled-without-secret": True,
    }:
        raise ContractFailure("MemeLoop Cloud signature contract changed")
    cloud_idempotency = cloud.get("x-idempotency-contract")
    if cloud_idempotency != {
        "namespace": "tenant-and-event-id-digest",
        "payload": "canonical-full-snapshot",
        "different-payload-status": 409,
        "stale-version-status": 409,
        "quota-version-source": "durable-subscription-entitlement",
        "policy-update": "compare-and-set-on-current-entitlement-version",
        "stable-history-owner": "key-id-and-account-id",
        "raw-event-id-persisted": False,
    }:
        raise ContractFailure("MemeLoop Cloud ordered idempotency contract changed")
    snapshot = document.get("components", {}).get("schemas", {}).get(
        "MemeLoopCloudSubscriptionSnapshot", {}
    )
    if snapshot.get("additionalProperties") is not False or set(
        snapshot.get("properties", {}).get("status", {}).get("enum", [])
    ) != {"active", "cancelled"}:
        raise ContractFailure("MemeLoop Cloud full snapshot schema changed")


def matching_rule(path: str, boundary: dict[str, Any]) -> dict[str, Any]:
    matches = []
    for rule in boundary["rules"]:
        exact_paths = rule.get("exact_paths", [])
        prefixes = rule.get("path_prefixes", [])
        if path in exact_paths or any(path.startswith(prefix) for prefix in prefixes):
            matches.append(rule)
    if len(matches) != 1:
        names = [rule.get("name", "<unnamed>") for rule in matches]
        raise ContractFailure(f"route {path} matched {len(matches)} boundary rules: {names}")
    return matches[0]


def check_contract(
    source: list[dict[str, str]],
    spec_routes: list[dict[str, Any]],
    spec: dict[str, Any],
    boundary: dict[str, Any],
) -> dict[str, Any]:
    validate_product_contracts(spec)
    source_by_key = {(route["method"], route["path"]): route for route in source}
    spec_by_key = {(route["method"], route["path"]): route for route in spec_routes}
    contract: list[dict[str, Any]] = []
    for key, source_route in source_by_key.items():
        rule = matching_rule(source_route["path"], boundary)
        if rule["source_role"] != source_route["source_role"]:
            raise ContractFailure(
                f"{key} is registered in {source_route['source_role']} but boundary says "
                f"{rule['source_role']}"
            )
        expected_runtime_roles = EXPECTED_RUNTIME_ROLES[source_route["source_role"]]
        if rule["runtime_roles"] != expected_runtime_roles:
            raise ContractFailure(
                f"boundary {rule['name']} runtime_roles={rule['runtime_roles']} but "
                f"source role {source_route['source_role']} requires {expected_runtime_roles}"
            )
        expected_in_spec = bool(rule["in_openapi"])
        actual_in_spec = key in spec_by_key
        if expected_in_spec != actual_in_spec:
            raise ContractFailure(
                f"{key} OpenAPI presence is {actual_in_spec}, boundary requires {expected_in_spec}"
            )
        item: dict[str, Any] = source_route | {
            "boundary": rule["name"],
            "runtime_roles": rule["runtime_roles"],
            "exposure": rule["exposure"],
            "authentication": rule["authentication"],
            "in_openapi": actual_in_spec,
        }
        if actual_in_spec:
            item |= {
                "operation_id": spec_by_key[key]["operation_id"],
                "schema_refs": spec_by_key[key]["schema_refs"],
            }
        contract.append(item)
    undocumented_source = sorted(set(spec_by_key) - set(source_by_key))
    if undocumented_source:
        raise ContractFailure(f"OpenAPI operations missing from Axum source: {undocumented_source}")
    path_count = len(spec["paths"])
    operation_count = len(spec_routes)
    minimum_paths = int(boundary["minimum_openapi_paths"])
    minimum_operations = int(boundary["minimum_openapi_operations"])
    if path_count < minimum_paths or operation_count < minimum_operations:
        raise ContractFailure(
            "OpenAPI surface regressed: "
            f"paths={path_count} (minimum {minimum_paths}), "
            f"operations={operation_count} (minimum {minimum_operations})"
        )
    private_violations = sorted(
        {
            item["path"]
            for item in contract
            if item["source_role"] == "control" and item["exposure"] != "private-only"
        }
    )
    if private_violations:
        raise ContractFailure(f"control routes are not private-only: {private_violations}")
    return {
        "schema_version": 1,
        "openapi_paths": path_count,
        "openapi_operations": operation_count,
        "source_operations": len(source),
        "excluded_source_operations": len(source) - operation_count,
        "routes": sorted(contract, key=lambda item: (item["path"], item["method"])),
    }


def parse_args() -> argparse.Namespace:
    repository = pathlib.Path(__file__).resolve().parents[2]
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--source", type=pathlib.Path, default=repository / "src/api")
    parser.add_argument("--openapi", type=pathlib.Path, default=repository / "openapi/openapi.yaml")
    parser.add_argument(
        "--boundaries",
        type=pathlib.Path,
        default=repository / "openapi/route-boundaries.json",
    )
    parser.add_argument("--output", type=pathlib.Path)
    return parser.parse_args()


def read_rust_source(path: pathlib.Path) -> str:
    if path.is_file():
        return path.read_text(encoding="utf-8")
    if not path.is_dir():
        raise ContractFailure(f"Rust API source does not exist: {path}")
    sources = sorted(path.rglob("*.rs"))
    if not sources:
        raise ContractFailure(f"Rust API source directory is empty: {path}")
    return "\n".join(
        f"// source: {source.relative_to(path).as_posix()}\n"
        f"{source.read_text(encoding='utf-8')}"
        for source in sources
    )


def main() -> int:
    args = parse_args()
    try:
        source = read_rust_source(args.source)
        spec = yaml.safe_load(args.openapi.read_text(encoding="utf-8"))
        boundary = json.loads(args.boundaries.read_text(encoding="utf-8"))
        if not isinstance(spec, dict) or not isinstance(boundary, dict):
            raise ContractFailure("OpenAPI and boundary documents must be objects")
        validate_spec(spec)
        report = check_contract(source_routes(source), openapi_routes(spec), spec, boundary)
        encoded = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(encoded, encoding="utf-8")
        print(
            "OpenAPI contract OK: "
            f"{report['openapi_paths']} paths, {report['openapi_operations']} operations, "
            f"{report['excluded_source_operations']} intentional source-only static routes"
        )
        return 0
    except (ContractFailure, OpenAPIValidationError, OSError, ValueError) as error:
        print(f"OpenAPI contract FAILED: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
