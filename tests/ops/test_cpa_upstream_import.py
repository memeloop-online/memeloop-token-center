#!/usr/bin/env python3
"""Black-box tests for the real CPA config/auth-dir upstream importer."""

from __future__ import annotations

import contextlib
import copy
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import threading
import unittest
import urllib.parse
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Iterator


REPOSITORY = pathlib.Path(__file__).resolve().parents[2]
IMPORTER = REPOSITORY / "ops" / "cpa-upstreams" / "import-cpa-upstreams.py"
FIXTURES = REPOSITORY / "tests" / "fixtures" / "cpa-upstreams"
SANITIZER = REPOSITORY / "tests" / "ops" / "sanitize-cpa-upstream-fixtures.py"
TARGET_TOKEN = "fixture-only-target-service-token"
BRIDGE_SECRET = "fixture-only-subscription-bridge-secret"


def account_view(
    *,
    name: str,
    driver: str,
    config: dict[str, object],
    tenant: str,
    account_id: str | None = None,
    generation: int = 1,
    status: str = "active",
    updated_at: int = 1_900_000_000_000,
) -> dict[str, object]:
    identifier = account_id or str(uuid.uuid5(uuid.NAMESPACE_URL, f"fixture:{tenant}:{name}"))
    return {
        "id": identifier,
        "tenant_external_id": tenant,
        "name": name,
        "driver": driver,
        "config": config,
        "status": status,
        "updated_at": updated_at,
        "credential_generation": generation,
    }


class TargetState:
    def __init__(self) -> None:
        self.accounts: dict[str, dict[str, object]] = {}
        self.account_names_by_id: dict[str, str] = {}
        self.idempotency: dict[str, tuple[bytes, str]] = {}
        self.subscription_names: dict[str, str] = {}
        self.managed_oauth_imports: dict[
            tuple[str, str, str], tuple[bytes, dict[str, object]]
        ] = {}
        self.managed_oauth_bodies: list[dict[str, object]] = []
        self.managed_oauth_capabilities = True
        self.managed_oauth_source_types = ["codex", "gemini-legacy"]
        self.managed_oauth_error_status: int | None = None
        self.managed_oauth_error_body: object = {"error": "fixture-only-reflected-secret"}
        self.requests: list[tuple[str, str]] = []
        self.authorization_headers: list[str | None] = []
        self.write_count = 0
        self.lock = threading.Lock()

    def add(self, account: dict[str, object]) -> None:
        name = str(account["name"])
        identifier = str(account["id"])
        self.accounts[name] = account
        self.account_names_by_id[identifier] = name


class MockTargetHandler(BaseHTTPRequestHandler):
    server: "MockTargetServer"

    def log_message(self, format: str, *args: object) -> None:
        del format, args

    def _body(self) -> bytes:
        length = int(self.headers.get("Content-Length", "0"))
        return self.rfile.read(length)

    def _json(self, status: int, value: object) -> None:
        body = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def _authorized(self) -> bool:
        authorization = self.headers.get("Authorization")
        self.server.state.authorization_headers.append(authorization)
        if authorization != f"Bearer {TARGET_TOKEN}":
            self._json(401, {"error": "unauthorized"})
            return False
        return True

    def _record(self) -> None:
        self.server.state.requests.append((self.command, self.path))

    def do_GET(self) -> None:  # noqa: N802
        with self.server.state.lock:
            self._record()
            if self.path.startswith("/redirect/"):
                self.send_response(302)
                self.send_header("Location", "/internal/v1/provider-types")
                self.end_headers()
                return
            if not self._authorized():
                return
            if self.path == "/internal/v1/provider-types":
                self._json(
                    200,
                    [
                        {"id": "http-json"},
                        {"id": "cpa-subscription-bridge"},
                    ],
                )
                return
            if self.path == "/internal/v1/imports/cpa/managed-oauth/capabilities":
                if not self.server.state.managed_oauth_capabilities:
                    self._json(404, {"error": "fixture-only-capability-error"})
                    return
                self._json(
                    200,
                    {
                        "contract_version": 1,
                        "source_types": self.server.state.managed_oauth_source_types,
                    },
                )
                return
            if self.path.startswith("/internal/v1/upstreams?"):
                self._json(200, list(self.server.state.accounts.values()))
                return
            self._json(404, {"error": "not_found"})

    def do_POST(self) -> None:  # noqa: N802
        with self.server.state.lock:
            self._record()
            if not self._authorized():
                return
            body = self._body()
            try:
                document = json.loads(body)
            except json.JSONDecodeError:
                self._json(400, {"error": "invalid"})
                return
            if self.path == "/internal/v1/upstreams":
                self.server.state.write_count += 1
                name = document["name"]
                if name in self.server.state.accounts:
                    self._json(409, {"error": "conflict"})
                    return
                account = account_view(
                    name=name,
                    driver=document["driver"],
                    config=document["config"],
                    tenant=document["tenant_external_id"],
                )
                self.server.state.add(account)
                self._json(201, account)
                return
            if self.path == "/internal/v1/imports/cpa/subscription-accounts":
                self.server.state.write_count += 1
                auth_file = document["auth_files"][0]
                auth_document = auth_file["document"]
                provider = auth_document["upstream"]
                handle = auth_document["handle"]
                stable = hashlib.sha256(
                    f"{document['tenant_external_id']}\0{provider}\0{handle}".encode()
                ).hexdigest()[:16]
                name = self.server.state.subscription_names.setdefault(
                    handle, f"cpa-{provider}-{stable}"
                )
                account = self.server.state.accounts.get(name)
                if account is None:
                    account = account_view(
                        name=name,
                        driver="cpa-subscription-bridge",
                        config={
                            "base_url": document["bridge_base_url"],
                            "provider": provider,
                            "network_scope": "private",
                        },
                        tenant=document["tenant_external_id"],
                    )
                    self.server.state.add(account)
                self._json(
                    201,
                    {
                        "imported": [
                            {
                                "source_fingerprint": "fixture-only-fingerprint",
                                "provider": provider,
                                "account": account,
                            }
                        ],
                        "skipped": [],
                    },
                )
                return
            if self.path == "/internal/v1/imports/cpa/managed-oauth":
                self.server.state.write_count += 1
                if self.server.state.managed_oauth_error_status is not None:
                    self._json(
                        self.server.state.managed_oauth_error_status,
                        self.server.state.managed_oauth_error_body,
                    )
                    return
                self.server.state.managed_oauth_bodies.append(copy.deepcopy(document))
                if set(document) != {
                    "contract_version",
                    "tenant_external_id",
                    "source",
                    "source_type",
                    "document",
                }:
                    self._json(400, {"error": "invalid_shape"})
                    return
                source = document["source"]
                if (
                    document["contract_version"] != 1
                    or not isinstance(source, dict)
                    or set(source) != {"kind", "relative_path"}
                    or source["kind"] != "auth_file"
                    or document["source_type"]
                    not in self.server.state.managed_oauth_source_types
                ):
                    self._json(400, {"error": "invalid_contract"})
                    return
                identity = (
                    str(document["tenant_external_id"]),
                    str(source["kind"]),
                    str(source["relative_path"]),
                )
                canonical = json.dumps(
                    {
                        "source_type": document["source_type"],
                        "document": document["document"],
                    },
                    sort_keys=True,
                    separators=(",", ":"),
                ).encode("utf-8")
                existing = self.server.state.managed_oauth_imports.get(identity)
                if existing is not None:
                    prior, account = existing
                    if prior != canonical:
                        self._json(409, {"error": "fixture-only-payload-digest-conflict"})
                        return
                    self._json(200, {"disposition": "replayed", "account": account})
                    return
                stable = hashlib.sha256("\0".join(identity).encode("utf-8")).hexdigest()[:16]
                account = account_view(
                    name=f"cpa-managed-{stable}",
                    driver=f"managed-{document['source_type']}",
                    config={"managed_oauth": True},
                    tenant=str(document["tenant_external_id"]),
                )
                self.server.state.add(account)
                self.server.state.managed_oauth_imports[identity] = (canonical, account)
                self._json(201, {"disposition": "created", "account": account})
                return
            self._json(404, {"error": "not_found"})

    def do_PUT(self) -> None:  # noqa: N802
        with self.server.state.lock:
            self._record()
            if not self._authorized():
                return
            self.server.state.write_count += 1
            body = self._body()
            idempotency_key = self.headers.get("Idempotency-Key")
            if not idempotency_key:
                self._json(400, {"error": "missing_idempotency_key"})
                return
            path_parts = self.path.split("/")
            if len(path_parts) != 6 or path_parts[-1] != "credential":
                self._json(404, {"error": "not_found"})
                return
            account_id = path_parts[-2]
            name = self.server.state.account_names_by_id.get(account_id)
            if name is None:
                self._json(404, {"error": "not_found"})
                return
            replay = self.server.state.idempotency.get(idempotency_key)
            if replay is not None:
                previous_body, previous_name = replay
                if previous_body != body or previous_name != name:
                    self._json(409, {"error": "idempotency_conflict"})
                    return
                self._json(200, self.server.state.accounts[name])
                return
            account = self.server.state.accounts[name]
            account["credential_generation"] = int(account["credential_generation"]) + 1
            account["updated_at"] = int(account["updated_at"]) + 1
            self.server.state.idempotency[idempotency_key] = (body, name)
            self._json(200, account)

    def do_PATCH(self) -> None:  # noqa: N802
        with self.server.state.lock:
            self._record()
            if not self._authorized():
                return
            self.server.state.write_count += 1
            body = json.loads(self._body())
            account_id = self.path.rsplit("/", 1)[-1]
            name = self.server.state.account_names_by_id.get(account_id)
            if name is None:
                self._json(404, {"error": "not_found"})
                return
            account = self.server.state.accounts[name]
            if account["updated_at"] != body["expected_updated_at"]:
                self._json(409, {"error": "stale"})
                return
            account["status"] = body["status"]
            account["updated_at"] = int(account["updated_at"]) + 1
            self._json(200, account)


class MockTargetServer(ThreadingHTTPServer):
    def __init__(self, address: tuple[str, int], state: TargetState) -> None:
        super().__init__(address, MockTargetHandler)
        self.state = state


@contextlib.contextmanager
def mock_target() -> Iterator[tuple[str, TargetState]]:
    state = TargetState()
    server = MockTargetServer(("127.0.0.1", 0), state)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"http://127.0.0.1:{server.server_port}", state
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=5)


class SecureSource:
    def __init__(self, fixture: str) -> None:
        self.temp = tempfile.TemporaryDirectory(prefix="mtc-cpa-upstream-test-")
        self.root = pathlib.Path(self.temp.name) / "source"
        shutil.copytree(FIXTURES / fixture, self.root)
        for directory in [self.root, *(path for path in self.root.rglob("*") if path.is_dir())]:
            directory.chmod(0o700)
        for file in (path for path in self.root.rglob("*") if path.is_file()):
            file.chmod(0o600)

    @property
    def config(self) -> pathlib.Path:
        return self.root / "config.yaml"

    @property
    def auth(self) -> pathlib.Path:
        return self.root / "auth"

    def secret_file(self, name: str, value: str) -> pathlib.Path:
        path = pathlib.Path(self.temp.name) / name
        path.write_text(value + "\n", encoding="utf-8")
        path.chmod(0o600)
        return path

    def close(self) -> None:
        self.temp.cleanup()


def run_importer(*arguments: object) -> subprocess.CompletedProcess[str]:
    command = [sys_executable(), str(IMPORTER), *(str(value) for value in arguments)]
    return subprocess.run(command, text=True, capture_output=True, check=False)


def sys_executable() -> str:
    return os.environ.get("PYTHON", "python3")


class CpaUpstreamImportTests(unittest.TestCase):
    def test_supported_fixture_dry_run_is_count_only_and_sanitized(self) -> None:
        sanitizer = subprocess.run(
            [sys_executable(), str(SANITIZER)], text=True, capture_output=True, check=False
        )
        self.assertEqual(sanitizer.returncode, 0, sanitizer.stderr)
        source = SecureSource("supported")
        self.addCleanup(source.close)
        result = run_importer(
            "--config",
            source.config,
            "--auth-dir",
            source.auth,
            "--bridge-base-url",
            "https://bridge.example.test",
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {
                "api_account_count": 6,
                "created_count": 0,
                "created_managed_oauth_count": 0,
                "disabled_source_count": 0,
                "imported_subscription_count": 0,
                "managed_oauth_account_count": 0,
                "managed_oauth_source_type_counts": {},
                "mode": "dry-run",
                "replayed_count": 0,
                "replayed_managed_oauth_count": 0,
                "subscription_account_count": 2,
            },
        )
        self.assertEqual(result.stderr, "")
        self.assertNotIn("fixture-only-", result.stdout)
        self.assertNotIn("FixtureCopilotHandle01", result.stdout)

    def test_apply_and_replay_create_no_duplicate_accounts(self) -> None:
        source = SecureSource("supported")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        bridge_secret_file = source.secret_file("bridge-secret", BRIDGE_SECRET)
        with mock_target() as (base_url, state):
            arguments = (
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--tenant",
                "fixture-tenant",
                "--bridge-base-url",
                f"{base_url}/bridge",
                "--bridge-secret-file",
                bridge_secret_file,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
            first = run_importer(*arguments)
            self.assertEqual(first.returncode, 0, first.stderr)
            first_summary = json.loads(first.stdout)
            self.assertEqual(first_summary["created_count"], 6)
            self.assertEqual(first_summary["replayed_count"], 0)
            self.assertEqual(first_summary["imported_subscription_count"], 2)
            self.assertEqual(len(state.accounts), 8)
            direct_generations = [
                account["credential_generation"]
                for account in state.accounts.values()
                if account["driver"] == "http-json"
            ]
            self.assertEqual(direct_generations, [2] * 6)

            second = run_importer(*arguments)
            self.assertEqual(second.returncode, 0, second.stderr)
            second_summary = json.loads(second.stdout)
            self.assertEqual(second_summary["created_count"], 0)
            self.assertEqual(second_summary["replayed_count"], 6)
            self.assertEqual(second_summary["imported_subscription_count"], 2)
            self.assertEqual(len(state.accounts), 8)
            direct_generations = [
                account["credential_generation"]
                for account in state.accounts.values()
                if account["driver"] == "http-json"
            ]
            self.assertEqual(direct_generations, [2] * 6)
            self.assertTrue(state.authorization_headers)
            self.assertEqual(set(state.authorization_headers), {f"Bearer {TARGET_TOKEN}"})
            for output in (first.stdout, first.stderr, second.stdout, second.stderr):
                self.assertNotIn(TARGET_TOKEN, output)
                self.assertNotIn(BRIDGE_SECRET, output)
                self.assertNotIn("fixture-only-cpa-", output)
                self.assertNotIn("FixtureCursorHandle01", output)

    def test_managed_oauth_dry_run_is_count_only_and_makes_no_target_request(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            result = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            json.loads(result.stdout),
            {
                "api_account_count": 1,
                "created_count": 0,
                "created_managed_oauth_count": 0,
                "disabled_source_count": 0,
                "imported_subscription_count": 0,
                "managed_oauth_account_count": 2,
                "managed_oauth_source_type_counts": {
                    "codex": 1,
                    "gemini-legacy": 1,
                },
                "mode": "dry-run",
                "replayed_count": 0,
                "replayed_managed_oauth_count": 0,
                "subscription_account_count": 0,
            },
        )
        self.assertEqual(result.stderr, "")
        self.assertEqual(state.requests, [])
        for forbidden in (
            "fixture-only-",
            "codex-user@example.test",
            "gemini-user@example.test",
            "codex-account.json",
            "gemini-account.json",
        ):
            self.assertNotIn(forbidden, result.stdout + result.stderr)

    def test_managed_oauth_apply_and_replay_use_capabilities_and_exact_requests(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            arguments = (
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--tenant",
                "managed-fixture",
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
            first = run_importer(*arguments)
            self.assertEqual(first.returncode, 0, first.stderr)
            self.assertEqual(
                state.requests[:3],
                [
                    ("GET", "/internal/v1/imports/cpa/managed-oauth/capabilities"),
                    ("GET", "/internal/v1/provider-types"),
                    ("GET", "/internal/v1/upstreams?tenant_external_id=managed-fixture"),
                ],
            )
            first_summary = json.loads(first.stdout)
            self.assertEqual(first_summary["created_managed_oauth_count"], 2)
            self.assertEqual(first_summary["replayed_managed_oauth_count"], 0)
            self.assertEqual(first_summary["created_count"], 1)
            self.assertEqual(len(state.accounts), 3)
            self.assertEqual(len(state.managed_oauth_bodies), 2)
            by_type = {
                str(body["source_type"]): body for body in state.managed_oauth_bodies
            }
            self.assertEqual(set(by_type), {"codex", "gemini-legacy"})
            for source_type, filename in (
                ("codex", "codex-account.json"),
                ("gemini-legacy", "gemini-account.json"),
            ):
                body = by_type[source_type]
                self.assertEqual(
                    set(body),
                    {
                        "contract_version",
                        "tenant_external_id",
                        "source",
                        "source_type",
                        "document",
                    },
                )
                self.assertEqual(body["contract_version"], 1)
                self.assertEqual(body["tenant_external_id"], "managed-fixture")
                self.assertEqual(
                    body["source"],
                    {"kind": "auth_file", "relative_path": filename},
                )
                fixture_document = json.loads((source.auth / filename).read_text("utf-8"))
                self.assertEqual(body["document"], fixture_document)

            generations = {
                name: account["credential_generation"]
                for name, account in state.accounts.items()
            }
            second = run_importer(*arguments)
            self.assertEqual(second.returncode, 0, second.stderr)
            second_summary = json.loads(second.stdout)
            self.assertEqual(second_summary["created_managed_oauth_count"], 0)
            self.assertEqual(second_summary["replayed_managed_oauth_count"], 2)
            self.assertEqual(second_summary["created_count"], 0)
            self.assertEqual(second_summary["replayed_count"], 1)
            self.assertEqual(len(state.accounts), 3)
            self.assertEqual(
                {
                    name: account["credential_generation"]
                    for name, account in state.accounts.items()
                },
                generations,
            )
            for output in (first.stdout, first.stderr, second.stdout, second.stderr):
                for forbidden in (
                    TARGET_TOKEN,
                    "fixture-only-",
                    "@example.test",
                    "codex-account.json",
                    "gemini-account.json",
                    "payload_digest",
                    "source_fingerprint",
                ):
                    self.assertNotIn(forbidden, output)

    def test_all_target_preflights_complete_before_mixed_snapshot_writes(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        for filename in ("copilot-account.json", "cursor-account.json"):
            copied = source.auth / filename
            shutil.copy2(FIXTURES / "supported" / "auth" / filename, copied)
            copied.chmod(0o600)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            result = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--tenant",
                "mixed-preflight",
                "--bridge-base-url",
                f"{base_url}/bridge",
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            state.requests[:3],
            [
                ("GET", "/internal/v1/imports/cpa/managed-oauth/capabilities"),
                ("GET", "/internal/v1/provider-types"),
                ("GET", "/internal/v1/upstreams?tenant_external_id=mixed-preflight"),
            ],
        )
        self.assertTrue(all(method == "GET" for method, _ in state.requests[:3]))
        summary = json.loads(result.stdout)
        self.assertEqual(summary["managed_oauth_account_count"], 2)
        self.assertEqual(summary["subscription_account_count"], 2)
        self.assertEqual(summary["api_account_count"], 1)

    def test_missing_managed_oauth_capability_stops_before_any_write(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            state.managed_oauth_capabilities = False
            result = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
        self.assertEqual(result.returncode, 2)
        self.assertEqual(
            state.requests,
            [("GET", "/internal/v1/imports/cpa/managed-oauth/capabilities")],
        )
        self.assertEqual(state.write_count, 0)
        self.assertEqual(result.stdout, "")
        self.assertNotIn("fixture-only-", result.stderr)

    def test_missing_required_managed_source_type_stops_before_any_write(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            state.managed_oauth_source_types = ["codex"]
            result = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
        self.assertEqual(result.returncode, 2)
        self.assertIn("missing a managed OAuth source type", result.stderr)
        self.assertEqual(state.write_count, 0)
        self.assertEqual(
            state.requests,
            [("GET", "/internal/v1/imports/cpa/managed-oauth/capabilities")],
        )

    def test_unknown_auth_type_fails_closed(self) -> None:
        source = SecureSource("unknown")
        self.addCleanup(source.close)
        result = run_importer("--config", source.config, "--auth-dir", source.auth)
        self.assertEqual(result.returncode, 2)
        self.assertIn("unsupported account type", result.stderr)
        self.assertNotIn("fixture-only-", result.stderr)

        unknown_document = source.auth / "unknown-account.json"
        unknown_document.write_text(
            json.dumps(
                {
                    "type": "future-oauth-provider",
                    "email": "future-user@example.test",
                    "access_token": "fixture-only-future-oauth-token",
                }
            ),
            encoding="utf-8",
        )
        unknown_document.chmod(0o600)
        unknown_oauth = run_importer(
            "--config", source.config, "--auth-dir", source.auth
        )
        self.assertEqual(unknown_oauth.returncode, 2)
        self.assertIn("unsupported managed OAuth type", unknown_oauth.stderr)
        for forbidden in (
            "fixture-only-future-oauth-token",
            "future-user@example.test",
            "unknown-account.json",
        ):
            self.assertNotIn(forbidden, unknown_oauth.stderr)

    def test_managed_oauth_preserves_utf8_posix_relative_path(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        nested = source.auth / "子目录"
        nested.mkdir(mode=0o700)
        shutil.move(source.auth / "codex-account.json", nested / "codex-account.json")
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            result = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
        self.assertEqual(result.returncode, 0, result.stderr)
        codex = next(
            body
            for body in state.managed_oauth_bodies
            if body["source_type"] == "codex"
        )
        self.assertEqual(
            codex["source"],
            {"kind": "auth_file", "relative_path": "子目录/codex-account.json"},
        )
        self.assertNotIn("子目录", result.stdout + result.stderr)
        self.assertNotIn("codex-account.json", result.stdout + result.stderr)

    def test_managed_oauth_rejects_non_posix_path_before_target_preflight(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        invalid_name = "private\\codex-account.json"
        shutil.move(source.auth / "codex-account.json", source.auth / invalid_name)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            result = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
        self.assertEqual(result.returncode, 2)
        self.assertIn("invalid relative path", result.stderr)
        self.assertEqual(state.requests, [])
        self.assertNotIn(invalid_name, result.stderr)

    def test_changed_managed_oauth_snapshot_returns_static_409_failure(self) -> None:
        source = SecureSource("oauth-blocked")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            arguments = (
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
            first = run_importer(*arguments)
            self.assertEqual(first.returncode, 0, first.stderr)
            before = {
                name: (account["credential_generation"], account["updated_at"])
                for name, account in state.accounts.items()
            }
            gemini_path = source.auth / "gemini-account.json"
            changed = json.loads(gemini_path.read_text(encoding="utf-8"))
            changed["token"]["refresh_token"] = "fixture-only-changed-refresh-token"
            gemini_path.write_text(json.dumps(changed), encoding="utf-8")
            gemini_path.chmod(0o600)
            replay = run_importer(*arguments)
        self.assertEqual(replay.returncode, 2)
        self.assertEqual(replay.stdout, "")
        self.assertIn("managed OAuth import returned an unexpected status", replay.stderr)
        for forbidden in (
            "fixture-only-changed-refresh-token",
            "fixture-only-payload-digest-conflict",
            "gemini-account.json",
            "gemini-user@example.test",
            "payload_digest",
        ):
            self.assertNotIn(forbidden, replay.stderr)
        self.assertEqual(len(state.accounts), 3)
        self.assertEqual(
            {
                name: (account["credential_generation"], account["updated_at"])
                for name, account in state.accounts.items()
            },
            before,
        )

    def test_managed_oauth_400_and_502_fail_without_reflecting_peer_body(self) -> None:
        for status in (400, 502):
            with self.subTest(status=status):
                source = SecureSource("oauth-blocked")
                self.addCleanup(source.close)
                token_file = source.secret_file("target-token", TARGET_TOKEN)
                with mock_target() as (base_url, state):
                    state.managed_oauth_error_status = status
                    state.managed_oauth_error_body = {
                        "error": "fixture-only-reflected-token",
                        "email": "codex-user@example.test",
                        "relative_path": "codex-account.json",
                        "payload_digest": "fixture-only-payload-digest",
                    }
                    result = run_importer(
                        "--config",
                        source.config,
                        "--auth-dir",
                        source.auth,
                        "--target-api-base-url",
                        base_url,
                        "--service-token-file",
                        token_file,
                        "--allow-http-loopback",
                        "--apply",
                    )
                self.assertEqual(result.returncode, 2)
                self.assertEqual(result.stdout, "")
                self.assertEqual(state.write_count, 1)
                self.assertIn(
                    "managed OAuth import returned an unexpected status", result.stderr
                )
                for forbidden in (
                    "fixture-only-reflected-token",
                    "codex-user@example.test",
                    "codex-account.json",
                    "payload_digest",
                ):
                    self.assertNotIn(forbidden, result.stderr)

    def test_permissions_symlink_duplicate_key_and_alias_are_rejected(self) -> None:
        source = SecureSource("supported")
        self.addCleanup(source.close)
        source.config.chmod(0o644)
        unsafe = run_importer("--config", source.config, "--auth-dir", source.auth)
        self.assertEqual(unsafe.returncode, 2)
        self.assertIn("mode-0600", unsafe.stderr)

        source.config.chmod(0o600)
        victim = source.auth / "copilot-account.json"
        victim.unlink()
        victim.symlink_to(source.auth / "cursor-account.json")
        symlinked = run_importer(
            "--config",
            source.config,
            "--auth-dir",
            source.auth,
            "--bridge-base-url",
            "https://bridge.example.test",
        )
        self.assertEqual(symlinked.returncode, 2)
        self.assertIn("symbolic link", symlinked.stderr)

        duplicate_source = SecureSource("unknown")
        self.addCleanup(duplicate_source.close)
        duplicate_source.config.write_text(
            'auth-dir: "/fixture/auth"\nauth-dir: "/duplicate/auth"\n',
            encoding="utf-8",
        )
        duplicate_source.config.chmod(0o600)
        duplicate = run_importer(
            "--config", duplicate_source.config, "--auth-dir", duplicate_source.auth
        )
        self.assertEqual(duplicate.returncode, 2)
        self.assertIn("duplicate mapping key", duplicate.stderr)

        alias_source = SecureSource("unknown")
        self.addCleanup(alias_source.close)
        alias_source.config.write_text(
            'auth-dir: &auth "/fixture/auth"\ncopy: *auth\n', encoding="utf-8"
        )
        alias_source.config.chmod(0o600)
        alias = run_importer(
            "--config", alias_source.config, "--auth-dir", alias_source.auth
        )
        self.assertEqual(alias.returncode, 2)
        self.assertIn("YAML aliases are not supported", alias.stderr)

    def test_http_requires_explicit_loopback_flag_and_redirect_is_not_followed(self) -> None:
        source = SecureSource("supported")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        with mock_target() as (base_url, state):
            insecure = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--bridge-base-url",
                "https://bridge.example.test",
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--apply",
            )
            self.assertEqual(insecure.returncode, 2)
            self.assertIn("must use HTTPS", insecure.stderr)
            self.assertEqual(state.requests, [])

            redirected = run_importer(
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--bridge-base-url",
                f"{base_url}/bridge",
                "--target-api-base-url",
                f"{base_url}/redirect",
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
            self.assertEqual(redirected.returncode, 2)
            self.assertIn("unexpected status", redirected.stderr)
            self.assertEqual(state.requests, [("GET", "/redirect/internal/v1/provider-types")])

    def test_existing_stable_identity_conflict_causes_no_writes(self) -> None:
        source = SecureSource("supported")
        self.addCleanup(source.close)
        token_file = source.secret_file("target-token", TARGET_TOKEN)
        bridge_secret_file = source.secret_file("bridge-secret", BRIDGE_SECRET)
        with mock_target() as (base_url, state):
            arguments = (
                "--config",
                source.config,
                "--auth-dir",
                source.auth,
                "--tenant",
                "fixture-conflict",
                "--bridge-base-url",
                f"{base_url}/bridge",
                "--bridge-secret-file",
                bridge_secret_file,
                "--target-api-base-url",
                base_url,
                "--service-token-file",
                token_file,
                "--allow-http-loopback",
                "--apply",
            )
            first = run_importer(*arguments)
            self.assertEqual(first.returncode, 0, first.stderr)
            direct = next(
                account for account in state.accounts.values() if account["driver"] == "http-json"
            )
            direct["config"] = {"base_url": "https://conflict.example.test"}
            before_writes = state.write_count
            conflict = run_importer(*arguments)
            self.assertEqual(conflict.returncode, 2)
            self.assertIn("conflicts with a stable CPA source identity", conflict.stderr)
            self.assertEqual(state.write_count, before_writes)


if __name__ == "__main__":
    unittest.main()
