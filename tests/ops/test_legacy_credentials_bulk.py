#!/usr/bin/env python3
from __future__ import annotations

import hashlib
import importlib.util
import json
import os
import pathlib
import re
import subprocess
import sys
import tempfile
import threading
import time
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest import mock


REPOSITORY = pathlib.Path(__file__).resolve().parents[2]
SCRIPT = REPOSITORY / "ops/legacy-credentials/attach-legacy-cpa-credentials.py"
FIXTURES = REPOSITORY / "tests/fixtures/legacy-credentials"
SPEC = importlib.util.spec_from_file_location("legacy_import", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
legacy_import = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = legacy_import
SPEC.loader.exec_module(legacy_import)


class TargetState:
    def __init__(self) -> None:
        self.mappings: dict[str, str] = {}
        self.post_count = 0
        self.get_count = 0
        self.lock = threading.Lock()


class FixtureHandler(BaseHTTPRequestHandler):
    state: TargetState
    fixture = (FIXTURES / "cpa-api-keys.json").read_bytes()

    def log_message(self, _format: str, *_args: object) -> None:
        return

    def do_GET(self) -> None:  # noqa: N802
        if (
            self.path != "/v0/management/api-keys"
            or self.headers.get("authorization") != "Bearer fixture-management-token"
        ):
            self.send_error(403)
            return
        with self.state.lock:
            self.state.get_count += 1
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(self.fixture)))
        self.end_headers()
        self.wfile.write(self.fixture)

    def do_POST(self) -> None:  # noqa: N802
        if self.headers.get("authorization") != "Bearer fixture-service-token":
            self.send_error(403)
            return
        prefix = "/internal/v1/keys/"
        suffix = "/legacy-credentials"
        if not self.path.startswith(prefix) or not self.path.endswith(suffix):
            self.send_error(404)
            return
        key_id = self.path[len(prefix) : -len(suffix)]
        length = int(self.headers.get("content-length", "0"))
        body = json.loads(self.rfile.read(length))
        source_hash = hashlib.sha256(body["credential"].encode()).hexdigest()
        if body.get("source_hash") != source_hash:
            self.send_error(400)
            return
        with self.state.lock:
            previous = self.state.mappings.get(source_hash)
            if previous is not None and previous != key_id:
                self.send_error(403)
                return
            self.state.mappings[source_hash] = key_id
            self.state.post_count += 1
        response = json.dumps(
            {
                "key_id": key_id,
                "generation": 0,
                "fingerprint": "fixture-fingerprint",
                "source_hash": source_hash,
            },
            separators=(",", ":"),
        ).encode()
        self.send_response(201)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(response)))
        self.end_headers()
        self.wfile.write(response)


class LegacyCredentialBulkTests(unittest.TestCase):
    def identity(self, credential: str, key_id: str) -> object:
        return legacy_import.Identity(
            hashlib.sha256(credential.encode()).hexdigest(), key_id
        )

    def test_plan_is_exact_one_to_one_and_replay_aware(self) -> None:
        first = "fixture-only-cpa-linux-codex-key-0001"
        second = "fixture-only-cpa-claude-code-key-0002"
        first_identity = self.identity(first, "10000000-0000-4000-8000-000000000001")
        second_identity = self.identity(second, "20000000-0000-4000-8000-000000000002")
        plan = legacy_import.build_plan(
            [second, first], [first_identity, second_identity], [first_identity]
        )
        self.assertEqual([item[1] for item in plan.candidates], [first_identity, second_identity])
        self.assertEqual(plan.already_attached, 1)

    def test_unmatched_missing_and_duplicate_inputs_fail_closed(self) -> None:
        first = "fixture-only-cpa-linux-codex-key-0001"
        second = "fixture-only-cpa-claude-code-key-0002"
        first_identity = self.identity(first, "10000000-0000-4000-8000-000000000001")
        second_identity = self.identity(second, "20000000-0000-4000-8000-000000000002")
        cases = (
            ([first], [first_identity, second_identity]),
            ([first, second], [first_identity]),
            ([first, first], [first_identity]),
        )
        for credentials, identities in cases:
            with self.subTest(credentials=len(credentials), identities=len(identities)):
                with self.assertRaises(legacy_import.ImportFailure):
                    legacy_import.build_plan(credentials, identities, [])

    def test_duplicate_target_and_existing_conflicts_fail_closed(self) -> None:
        first = "fixture-only-cpa-linux-codex-key-0001"
        second = "fixture-only-cpa-claude-code-key-0002"
        first_identity = self.identity(first, "10000000-0000-4000-8000-000000000001")
        duplicate_target = self.identity(second, first_identity.key_id)
        with self.assertRaises(legacy_import.ImportFailure):
            legacy_import.build_plan(
                [first, second], [first_identity, duplicate_target], []
            )
        conflicting_existing = legacy_import.Identity(
            first_identity.source_hash, "30000000-0000-4000-8000-000000000003"
        )
        with self.assertRaises(legacy_import.ImportFailure):
            legacy_import.build_plan([first], [first_identity], [conflicting_existing])
        with self.assertRaises(legacy_import.ImportFailure):
            legacy_import.build_plan([first], [first_identity], [], [first_identity])

    def test_parser_rejects_duplicate_json_fields_and_secret_whitespace(self) -> None:
        with self.assertRaises(legacy_import.ImportFailure):
            legacy_import.parse_candidates(
                b'{"api-keys":["fixture-only-key-00000001"],"api-keys":[]}',
                "cpa-json",
            )
        with self.assertRaises(legacy_import.ImportFailure):
            legacy_import.parse_candidates(b" fixture-only-key-00000001\n", "lines")

    def test_file_input_rejects_other_read_access(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            exposed = pathlib.Path(temporary) / "exposed-api-keys.json"
            exposed.write_bytes(FixtureHandler.fixture)
            exposed.chmod(0o644)
            with self.assertRaises(legacy_import.ImportFailure):
                legacy_import.read_secret_file(str(exposed), "credential input")
            exposed.chmod(0o440)
            self.assertEqual(
                legacy_import.read_secret_file(str(exposed), "credential input"),
                FixtureHandler.fixture,
            )

    def test_psql_connection_loss_is_reaped_and_reported_without_stderr(self) -> None:
        credential = "fixture-only-cpa-linux-codex-key-0001"
        source_hash = hashlib.sha256(credential.encode()).hexdigest()
        key_id = "10000000-0000-4000-8000-000000000001"
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            fake_psql = root / "psql"
            fake_psql.write_text(
                "#!/usr/bin/env python3\n"
                "import os, sys, time\n"
                "for line in sys.stdin:\n"
                "    if 'pg_try_advisory_lock' in line:\n"
                "        print('1', flush=True)\n"
                "    elif line.startswith('SELECT json_build_array'):\n"
                "        sys.stdout.close()\n"
                "        time.sleep(0.05)\n"
                "        print(os.environ['FAKE_PSQL_PRIVATE_STDERR'], file=sys.stderr, flush=True)\n"
                "        raise SystemExit(2)\n"
            )
            fake_psql.chmod(0o500)
            candidate_file = root / "api-keys.json"
            candidate_file.write_bytes(FixtureHandler.fixture)
            candidate_file.chmod(0o400)
            environment = os.environ.copy()
            environment["FAKE_PSQL_PRIVATE_STDERR"] = " ".join(
                (credential, source_hash, key_id)
            )
            result = subprocess.run(
                [
                    "python3",
                    str(SCRIPT),
                    "--tenant-external-id",
                    "fixture-tenant",
                    "--input-file",
                    str(candidate_file),
                    "--psql-binary",
                    str(fake_psql),
                ],
                check=False,
                capture_output=True,
                text=True,
                env=environment,
                timeout=20,
            )
            self.assertEqual(result.returncode, 2)
            self.assertIn(
                "PostgreSQL identity connection was lost (psql status 2)",
                result.stderr,
            )
            combined_output = result.stdout + result.stderr
            for forbidden in (credential, source_hash, key_id):
                self.assertNotIn(forbidden, combined_output)

    def test_psql_json_framing_rejects_control_marker_and_non_string_fields(self) -> None:
        credential = "fixture-only-cpa-linux-codex-key-0001"
        source_hash = hashlib.sha256(credential.encode()).hexdigest()
        key_id = "10000000-0000-4000-8000-000000000001"
        cases = (
            (
                json.dumps(
                    ["identity", f"invalid\n{legacy_import.IDENTITIES_END}\t", key_id]
                ),
                "invalid source hash",
            ),
            (json.dumps(["identity", source_hash, f"{key_id}\t"]), "invalid target key id"),
            (json.dumps(["identity", source_hash, 7]), "identity output is invalid"),
        )
        for output_row, expected_error in cases:
            with self.subTest(expected_error=expected_error):
                with tempfile.TemporaryDirectory() as temporary:
                    root = pathlib.Path(temporary)
                    fake_psql = root / "psql"
                    fake_psql.write_text(
                        "#!/usr/bin/env python3\n"
                        "import os, sys\n"
                        "for line in sys.stdin:\n"
                        "    if 'pg_try_advisory_lock' in line:\n"
                        "        print('1', flush=True)\n"
                        "    elif line.startswith('SELECT json_build_array'):\n"
                        "        print(os.environ['FAKE_PSQL_ROW'], flush=True)\n"
                        "    elif line.startswith('\\\\echo __MTC_LEGACY_IDENTITIES_END__'):\n"
                        "        print('__MTC_LEGACY_IDENTITIES_END__', flush=True)\n"
                        "    elif line.startswith('\\\\quit'):\n"
                        "        break\n"
                    )
                    fake_psql.chmod(0o500)
                    candidate_file = root / "api-keys.json"
                    candidate_file.write_bytes(FixtureHandler.fixture)
                    candidate_file.chmod(0o400)
                    environment = os.environ.copy()
                    environment["FAKE_PSQL_ROW"] = output_row
                    result = subprocess.run(
                        [
                            "python3",
                            str(SCRIPT),
                            "--tenant-external-id",
                            "fixture-tenant",
                            "--input-file",
                            str(candidate_file),
                            "--psql-binary",
                            str(fake_psql),
                        ],
                        check=False,
                        capture_output=True,
                        text=True,
                        env=environment,
                        timeout=20,
                    )
                self.assertEqual(result.returncode, 2)
                self.assertIn(expected_error, result.stderr)
                self.assertEqual(result.stdout, "")
                for forbidden in (credential, source_hash, key_id, output_row):
                    self.assertNotIn(forbidden, result.stderr)

    def test_read_only_cpa_management_export_uses_secret_file(self) -> None:
        state = TargetState()
        FixtureHandler.state = state
        server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            with tempfile.TemporaryDirectory() as temporary:
                token_file = pathlib.Path(temporary) / "management-token"
                token_file.write_text("fixture-management-token\n")
                token_file.chmod(0o400)
                value = legacy_import.fetch_cpa_candidates(
                    f"http://127.0.0.1:{server.server_port}",
                    str(token_file),
                    None,
                    True,
                )
            self.assertEqual(
                legacy_import.parse_candidates(value, "cpa-json"),
                legacy_import.parse_candidates(FixtureHandler.fixture, "cpa-json"),
            )
            self.assertEqual(state.get_count, 1)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()

    def test_apply_stops_after_holder_exits_or_stops_heartbeat(self) -> None:
        credentials = legacy_import.parse_candidates(FixtureHandler.fixture, "cpa-json")
        hashes = [hashlib.sha256(item.encode()).hexdigest() for item in credentials]
        key_ids = [
            "10000000-0000-4000-8000-000000000001",
            "20000000-0000-4000-8000-000000000002",
        ]
        rows = json.dumps(
            [
                ["identity", hashes[0], key_ids[0]],
                ["identity", hashes[1], key_ids[1]],
            ],
            separators=(",", ":"),
        )
        for behavior, expected_error in (
            ("exit", "PostgreSQL identity connection was lost (psql status 2)"),
            ("stall", "PostgreSQL identity heartbeat timed out"),
        ):
            with self.subTest(behavior=behavior):
                state = TargetState()

                class HolderLossHandler(FixtureHandler):
                    holder_stop: pathlib.Path

                    def do_POST(self) -> None:  # noqa: N802
                        self.holder_stop.write_text("stop")
                        time.sleep(0.1)
                        super().do_POST()

                HolderLossHandler.state = state
                server = ThreadingHTTPServer(("127.0.0.1", 0), HolderLossHandler)
                thread = threading.Thread(target=server.serve_forever, daemon=True)
                thread.start()
                try:
                    with tempfile.TemporaryDirectory() as temporary:
                        root = pathlib.Path(temporary)
                        holder_stop = root / "stop-holder"
                        HolderLossHandler.holder_stop = holder_stop
                        fake_psql = root / "psql"
                        fake_psql.write_text(
                            "#!/usr/bin/env python3\n"
                            "import json, os, pathlib, sys, time\n"
                            "stop = pathlib.Path(os.environ['FAKE_PSQL_STOP'])\n"
                            "for line in sys.stdin:\n"
                            "    if 'pg_try_advisory_lock' in line:\n"
                            "        print('1', flush=True)\n"
                            "    elif line.startswith('SELECT json_build_array'):\n"
                            "        for row in json.loads(os.environ['FAKE_PSQL_ROWS']):\n"
                            "            print(json.dumps(row, separators=(',', ':')), flush=True)\n"
                            "    elif line.startswith('\\\\echo __MTC_LEGACY_IDENTITIES_END__'):\n"
                            "        print('__MTC_LEGACY_IDENTITIES_END__', flush=True)\n"
                            "    elif '__MTC_LEGACY_IDENTITY_HEARTBEAT__' in line:\n"
                            "        if stop.exists():\n"
                            "            if os.environ['FAKE_PSQL_BEHAVIOR'] == 'exit':\n"
                            "                raise SystemExit(2)\n"
                            "            while True:\n"
                            "                time.sleep(60)\n"
                            "        print('__MTC_LEGACY_IDENTITY_HEARTBEAT__', flush=True)\n"
                        )
                        fake_psql.chmod(0o500)
                        candidate_file = root / "api-keys.json"
                        candidate_file.write_bytes(FixtureHandler.fixture)
                        candidate_file.chmod(0o400)
                        token_file = root / "service-token"
                        token_file.write_text("fixture-service-token\n")
                        token_file.chmod(0o400)
                        arguments = legacy_import.argument_parser().parse_args(
                            [
                                "--tenant-external-id",
                                "fixture-tenant",
                                "--input-file",
                                str(candidate_file),
                                "--psql-binary",
                                str(fake_psql),
                                "--apply",
                                "--target-api-base-url",
                                f"http://127.0.0.1:{server.server_port}",
                                "--allow-http-target",
                                "--service-token-file",
                                str(token_file),
                            ]
                        )
                        environment = {
                            "FAKE_PSQL_ROWS": rows,
                            "FAKE_PSQL_STOP": str(holder_stop),
                            "FAKE_PSQL_BEHAVIOR": behavior,
                        }
                        with (
                            mock.patch.dict(os.environ, environment),
                            mock.patch.object(
                                legacy_import, "HEARTBEAT_TIMEOUT_SECONDS", 0.2
                            ),
                            self.assertRaisesRegex(
                                legacy_import.ImportFailure, re.escape(expected_error)
                            ) as failure,
                        ):
                            legacy_import.run(arguments)
                    self.assertEqual(state.post_count, 1)
                    for forbidden in credentials + hashes + key_ids:
                        self.assertNotIn(forbidden, str(failure.exception))
                finally:
                    server.shutdown()
                    server.server_close()
                    thread.join()

    def test_cli_defaults_to_dry_run_then_apply_replays_without_secret_output(self) -> None:
        state = TargetState()
        FixtureHandler.state = state
        server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        fixture_credentials = legacy_import.parse_candidates(
            FixtureHandler.fixture, "cpa-json"
        )
        forbidden_output = fixture_credentials + [
            hashlib.sha256(item.encode()).hexdigest() for item in fixture_credentials
        ]
        try:
            with tempfile.TemporaryDirectory() as temporary:
                root = pathlib.Path(temporary)
                fake_psql = root / "psql"
                rows_file = FIXTURES / "cpamp-identities.csv"
                fake_psql.write_text(
                    "#!/usr/bin/env python3\n"
                    "import csv, json, os, pathlib, signal, sys\n"
                    "script_log = pathlib.Path(os.environ['FAKE_PSQL_SCRIPT_LOG'])\n"
                    "def terminate(_signal, _frame):\n"
                    "    with script_log.open('a') as output:\n"
                    "        output.write('\\0')\n"
                    "    with pathlib.Path(os.environ['FAKE_PSQL_CLOSE_LOG']).open('a') as output:\n"
                    "        output.write('closed\\n')\n"
                    "    raise SystemExit(0)\n"
                    "signal.signal(signal.SIGTERM, terminate)\n"
                    "for line in sys.stdin:\n"
                    "    with script_log.open('a') as output:\n"
                    "        output.write(line)\n"
                    "    if 'pg_try_advisory_lock' in line:\n"
                    "        print('1', flush=True)\n"
                    "    elif line.startswith('SELECT json_build_array'):\n"
                    "        rows = pathlib.Path(os.environ['FAKE_PSQL_ROWS']).read_text().splitlines()\n"
                    "        for row in csv.reader(rows):\n"
                    "            print(json.dumps(row, separators=(',', ':')), flush=True)\n"
                    "    elif line.startswith('\\\\echo __MTC_LEGACY_IDENTITIES_END__'):\n"
                    "        print('__MTC_LEGACY_IDENTITIES_END__', flush=True)\n"
                    "    elif '__MTC_LEGACY_IDENTITY_HEARTBEAT__' in line:\n"
                    "        print('__MTC_LEGACY_IDENTITY_HEARTBEAT__', flush=True)\n"
                )
                fake_psql.chmod(0o500)
                service_token_file = root / "service-token"
                service_token_file.write_text("fixture-service-token\n")
                service_token_file.chmod(0o400)
                candidate_file = root / "api-keys.json"
                candidate_file.write_bytes(FixtureHandler.fixture)
                candidate_file.chmod(0o400)
                common = [
                    "python3",
                    str(SCRIPT),
                    "--tenant-external-id",
                    "fixture-tenant",
                    "--input-file",
                    str(candidate_file),
                    "--psql-binary",
                    str(fake_psql),
                ]
                environment = os.environ.copy()
                environment["FAKE_PSQL_ROWS"] = str(rows_file)
                script_log = root / "psql-scripts"
                close_log = root / "psql-closes"
                environment["FAKE_PSQL_SCRIPT_LOG"] = str(script_log)
                environment["FAKE_PSQL_CLOSE_LOG"] = str(close_log)
                dry_run = subprocess.run(
                    common,
                    check=False,
                    capture_output=True,
                    text=True,
                    env=environment,
                    timeout=20,
                )
                self.assertEqual(dry_run.returncode, 0, dry_run.stderr)
                dry_summary = json.loads(dry_run.stdout)
                self.assertEqual(dry_summary["mode"], "dry-run")
                self.assertEqual(dry_summary["pending_count"], 2)
                self.assertEqual(state.post_count, 0)

                apply = common + [
                    "--apply",
                    "--target-api-base-url",
                    f"http://127.0.0.1:{server.server_port}",
                    "--allow-http-target",
                    "--service-token-file",
                    str(service_token_file),
                ]
                first_apply = subprocess.run(
                    apply,
                    check=False,
                    capture_output=True,
                    text=True,
                    env=environment,
                    timeout=20,
                )
                second_apply = subprocess.run(
                    apply,
                    check=False,
                    capture_output=True,
                    text=True,
                    env=environment,
                    timeout=20,
                )
                self.assertEqual(first_apply.returncode, 0, first_apply.stderr)
                self.assertEqual(second_apply.returncode, 0, second_apply.stderr)
                self.assertEqual(json.loads(first_apply.stdout)["attached_verified_count"], 2)
                self.assertEqual(first_apply.stdout, second_apply.stdout)
                self.assertEqual(state.post_count, 4)
                self.assertEqual(len(state.mappings), 2)
                scripts = script_log.read_text().split("\0")[:-1]
                self.assertEqual(len(scripts), 3)
                self.assertEqual(close_log.read_text().splitlines(), ["closed"] * 3)
                for index, script in enumerate(scripts):
                    self.assertEqual(script.count("pg_try_advisory_lock"), 1)
                    self.assertEqual(script.count("SELECT json_build_array"), 1)
                    self.assertEqual(script.count(legacy_import.IDENTITIES_END), 1)
                    self.assertEqual(
                        script.count(legacy_import.IDENTITY_HEARTBEAT),
                        1 if index == 0 else 5,
                    )
                    self.assertTrue(
                        script.endswith("SELECT '__MTC_LEGACY_IDENTITY_HEARTBEAT__';\n")
                    )
                    self.assertNotIn("\\watch", script)
                    self.assertNotIn("pg_advisory_unlock", script)
                    self.assertNotIn("\\quit", script)
                combined_output = "".join(
                    (
                        dry_run.stdout,
                        dry_run.stderr,
                        first_apply.stdout,
                        first_apply.stderr,
                        second_apply.stdout,
                        second_apply.stderr,
                    )
                )
                for forbidden in forbidden_output:
                    self.assertNotIn(forbidden, combined_output)
        finally:
            server.shutdown()
            server.server_close()
            thread.join()


if __name__ == "__main__":
    unittest.main(verbosity=2)
