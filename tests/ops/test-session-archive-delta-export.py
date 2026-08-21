#!/usr/bin/env python3
"""Black-box tests for native and legacy session archive delta sources."""

from __future__ import annotations

import base64
import datetime as dt
import fcntl
import hashlib
import http.server
import importlib.util
import json
import os
import shutil
import stat
import subprocess
import sys
import tempfile
import threading
import unittest
import urllib.parse
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
EXPORTER = ROOT / "ops" / "export-cpa-session-archive-delta.py"
TOKEN = "management-token-that-must-never-appear"
STABLE_CURSOR_PROTOCOL = "session-snapshot-cursor-v1"

SPEC = importlib.util.spec_from_file_location("mtc_archive_delta_export", EXPORTER)
assert SPEC is not None and SPEC.loader is not None
DELTA_EXPORT = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = DELTA_EXPORT
SPEC.loader.exec_module(DELTA_EXPORT)


def record(
    request_id: str,
    session_id: str,
    started_at: str,
    completed_at: str,
) -> dict[str, Any]:
    return {
        "schema_version": 2,
        "session_id": session_id,
        "request_id": request_id,
        "started_at": started_at,
        "completed_at": completed_at,
        "key_id": "key-hash",
        "principal_id": "principal",
        "requested_model": "model",
        "model": "model",
        "outcome": "success",
        "status_code": 200,
        "request": {"prompt": "payload-secret-that-must-never-appear"},
        "response": {"answer": request_id},
    }


def summary(session_id: str, rows: list[dict[str, Any]]) -> dict[str, Any]:
    return {
        "session_id": session_id,
        "requests": len(rows),
        "first_at": min(row["started_at"] for row in rows),
        "last_at": max(row["completed_at"] for row in rows),
        "key_id": "key-hash",
        "model": "model",
        "project": "project",
    }


def session_set_digest(sessions: list[dict[str, Any]]) -> str:
    stable = sorted(
        (
            {
                "session_id": item["session_id"],
                "requests": item["requests"],
                "first_at": item["first_at"].replace("Z", ".000000Z")
                if "." not in item["first_at"]
                else item["first_at"],
                "last_at": item["last_at"].replace("Z", ".000000Z")
                if "." not in item["last_at"]
                else item["last_at"],
                **(
                    {"records_sha256": item["records_sha256"]}
                    if "records_sha256" in item
                    else {}
                ),
            }
            for item in sessions
        ),
        key=lambda item: item["session_id"],
    )
    encoded = json.dumps(
        stable, ensure_ascii=False, sort_keys=True, separators=(",", ":")
    ).encode("utf-8")
    return hashlib.sha256(encoded).hexdigest()


def records_digest(rows: list[dict[str, Any]]) -> str:
    digest = hashlib.sha256()
    for row in sorted(rows, key=lambda item: item["request_id"]):
        digest.update(
            json.dumps(
                row, ensure_ascii=False, sort_keys=True, separators=(",", ":")
            ).encode("utf-8")
            + b"\n"
        )
    return digest.hexdigest()


def normalized_time(value: str) -> str:
    return value.replace("Z", ".000000Z") if "." not in value else value


class SourceState:
    def __init__(self) -> None:
        self.sessions: list[dict[str, Any]] = []
        self.session_snapshots: list[list[dict[str, Any]]] = []
        self.exports: dict[str, list[dict[str, Any]]] = {}
        self.stats_values: list[int] = []
        self.session_calls = 0
        self.legacy_session_calls = 0
        self.stats_calls = 0
        self.export_calls = 0
        self.ticket_calls = 0
        self.leak_calls = 0
        self.ticket_authorization_seen = False
        self.redirect_sessions = False
        self.cross_origin_ticket = False
        self.plugin_envelope = False
        self.stable_cursor_supported = False
        self.stable_snapshots: dict[
            str,
            tuple[list[dict[str, Any]], dict[str, list[dict[str, Any]]], str],
        ] = {}
        self.stable_snapshot_sequence = 0
        self.after_snapshot_sessions: list[dict[str, Any]] = []
        self.after_snapshot_exports: dict[str, list[dict[str, Any]]] = {}
        self.stable_gap_offset: int | None = None
        self.stable_cursor_loop = False
        self.stable_ticket_snapshots_seen: list[str] = []
        self.ingest_sequence = 0
        self.record_ingest_sequence: dict[str, int] = {}
        self.serve_live_snapshot_exports = False
        self.offline_full_enabled = False
        self.direct_ticket_targets: dict[str, tuple[str, str]] = {}
        self.direct_ticket_url: str | None = None
        self.expire_direct_ticket_once = False
        self.stable_transient_statuses: list[int] = []
        self.direct_authorization_seen = False
        self.expire_stable_replay_once = False
        self.ticket_whitespace_bytes = 0

    @property
    def request_count(self) -> int:
        return (
            self.session_calls
            + self.stats_calls
            + self.export_calls
            + self.ticket_calls
            + self.leak_calls
        )


class SourceHandler(http.server.BaseHTTPRequestHandler):
    server: "SourceServer"

    def log_message(self, format_string: str, *args: Any) -> None:
        return

    def send_json(self, value: Any, status_code: int = 200) -> None:
        body = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_response(status_code)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def management_authorized(self) -> bool:
        if self.headers.get("Authorization") == f"Bearer {TOKEN}":
            return True
        self.send_json({"error": "unauthorized"}, 401)
        return False

    def send_management_json(self, value: Any) -> None:
        if not self.server.state.plugin_envelope:
            self.send_json(value)
            return
        body = json.dumps(value, separators=(",", ":")).encode("utf-8")
        self.send_json(
            {
                "ok": True,
                "result": {
                    "StatusCode": 200,
                    "Headers": {"Content-Type": ["application/json"]},
                    "Body": base64.b64encode(body).decode("ascii"),
                },
            }
        )

    def do_GET(self) -> None:  # noqa: N802 - stdlib handler API
        state = self.server.state
        parsed = urllib.parse.urlsplit(self.path)
        sessions_path = "/v0/management/plugins/cpa-session-archive/sessions"
        stats_path = "/v0/management/plugins/cpa-session-archive/stats"
        export_path = "/v0/management/plugins/cpa-session-archive/export"
        direct_sessions_path = "/v1/sessions"
        direct_stats_path = "/v1/stats"
        direct_export_path = "/v1/export-tickets"
        if parsed.path == "/leak":
            state.leak_calls += 1
            self.send_json({"authorization": self.headers.get("Authorization")})
            return
        if parsed.path in {sessions_path, direct_sessions_path}:
            direct_sessions = parsed.path == direct_sessions_path
            state.session_calls += 1
            if direct_sessions:
                state.direct_authorization_seen |= bool(
                    self.headers.get("Authorization")
                )
            elif not self.management_authorized():
                return
            if state.redirect_sessions:
                self.send_response(302)
                self.send_header("Location", "/leak")
                self.end_headers()
                return
            query = urllib.parse.parse_qs(parsed.query)
            if query.get("cursor_protocol", [""])[0] == STABLE_CURSOR_PROTOCOL:
                if state.stable_transient_statuses:
                    status = state.stable_transient_statuses.pop(0)
                    self.send_response(status)
                    self.send_header("Retry-After", "0")
                    self.end_headers()
                    return
                if not state.stable_cursor_supported:
                    # v0.7.21 treats unknown query parameters as facet filters.
                    self.send_management_json({"sessions": []})
                    return
                limit = int(query.get("limit", ["100"])[0])
                lower_bound = query.get("lower_bound_completed_at", [""])[0]
                after_fence = query.get("after_ingest_fence", [""])[0]
                snapshot = query.get("snapshot", [""])[0]
                if not snapshot:
                    for rows in state.exports.values():
                        for row in rows:
                            request_id = row["request_id"]
                            if request_id in state.record_ingest_sequence:
                                continue
                            state.ingest_sequence += 1
                            state.record_ingest_sequence[request_id] = (
                                state.ingest_sequence
                            )
                    state.stable_snapshot_sequence += 1
                    snapshot = f"snapshot-{state.stable_snapshot_sequence}"
                    after_sequence = (
                        int(after_fence)
                        if after_fence
                        else None
                    )
                    ordered = sorted(
                        (
                            {
                                **dict(item),
                                "first_at": normalized_time(item["first_at"]),
                                "last_at": normalized_time(item["last_at"]),
                                "records_sha256": records_digest(
                                    state.exports.get(item["session_id"], [])
                                ),
                            }
                            for item in state.sessions
                            if after_sequence is None
                            or item["last_at"] >= lower_bound
                            or any(
                                state.record_ingest_sequence[row["request_id"]]
                                > after_sequence
                                for row in state.exports.get(item["session_id"], [])
                            )
                        ),
                        key=lambda item: item["session_id"],
                    )
                    ordered.sort(key=lambda item: item["last_at"], reverse=True)
                    snapshot_exports = {
                        key: [dict(row) for row in rows]
                        for key, rows in state.exports.items()
                        if key in {item["session_id"] for item in ordered}
                    }
                    ingest_fence = str(state.ingest_sequence)
                    state.stable_snapshots[snapshot] = (
                        ordered,
                        snapshot_exports,
                        ingest_fence,
                    )
                    if state.after_snapshot_sessions or state.after_snapshot_exports:
                        pending_sessions = {
                            item["session_id"]: item
                            for item in state.after_snapshot_sessions
                        }
                        state.sessions = [
                            pending_sessions.pop(item["session_id"], item)
                            for item in state.sessions
                        ]
                        state.sessions.extend(pending_sessions.values())
                        state.exports.update(state.after_snapshot_exports)
                        state.after_snapshot_sessions = []
                        state.after_snapshot_exports = {}
                stored = state.stable_snapshots.get(snapshot)
                if snapshot and direct_sessions and state.expire_stable_replay_once:
                    state.expire_stable_replay_once = False
                    state.stable_snapshots.pop(snapshot, None)
                    self.send_json({"error": "snapshot expired"}, 410)
                    return
                if stored is None:
                    self.send_json(
                        {"error": "snapshot expired"},
                        410 if direct_sessions else 409,
                    )
                    return
                all_sessions = stored[0]
                cursor = query.get("cursor", [""])[0]
                if cursor:
                    try:
                        offset = int(cursor.removeprefix("cursor-"))
                    except ValueError:
                        self.send_json({"error": "cursor"}, 400)
                        return
                else:
                    offset = 0
                end = min(offset + limit, len(all_sessions))
                page = all_sessions[offset:end]
                if state.stable_gap_offset is not None and offset <= state.stable_gap_offset < end:
                    page = [
                        item
                        for index, item in enumerate(page, start=offset)
                        if index != state.stable_gap_offset
                    ]
                complete = end == len(all_sessions)
                next_cursor = None if complete else f"cursor-{end}"
                if state.stable_cursor_loop and offset > 0 and not complete:
                    next_cursor = cursor
                self.send_management_json(
                    {
                        "cursor_protocol": STABLE_CURSOR_PROTOCOL,
                        "snapshot": snapshot,
                        "ingest_fence": stored[2],
                        "session_count": len(all_sessions),
                        "request_count": sum(
                            item["requests"] for item in all_sessions
                        ),
                        "session_set_sha256": session_set_digest(all_sessions),
                        "sessions": page,
                        "complete": complete,
                        "next_cursor": next_cursor,
                    }
                )
                return
            state.legacy_session_calls += 1
            snapshots = state.session_snapshots or [state.sessions]
            index = min(state.legacy_session_calls - 1, len(snapshots) - 1)
            query = urllib.parse.parse_qs(parsed.query)
            limit = int(query.get("limit", ["100"])[0])
            self.send_management_json({"sessions": snapshots[index][:limit]})
            return
        if parsed.path in {stats_path, direct_stats_path}:
            state.stats_calls += 1
            if parsed.path == direct_stats_path:
                state.direct_authorization_seen |= bool(
                    self.headers.get("Authorization")
                )
            elif not self.management_authorized():
                return
            default_count = sum(len(rows) for rows in state.exports.values())
            values = state.stats_values or [default_count]
            index = min(state.stats_calls - 1, len(values) - 1)
            response = {"records": values[index], "sessions": len(state.sessions)}
            if parsed.path == direct_stats_path:
                response.update(
                    {
                        "session_cursor_protocols": [STABLE_CURSOR_PROTOCOL],
                        "offline_full_snapshot_enabled": state.offline_full_enabled,
                    }
                )
            self.send_management_json(response)
            return
        if parsed.path in {export_path, direct_export_path}:
            state.export_calls += 1
            if parsed.path == direct_export_path:
                state.direct_authorization_seen |= bool(
                    self.headers.get("Authorization")
                )
            elif not self.management_authorized():
                return
            query = urllib.parse.parse_qs(parsed.query)
            direct = parsed.path == direct_export_path
            session_id = query.get("session_id" if direct else "id", [""])[0]
            snapshot = query.get("snapshot", [""])[0]
            exports = state.exports
            if snapshot:
                stored = state.stable_snapshots.get(snapshot)
                if stored is None:
                    self.send_json({"error": "snapshot expired"}, 409)
                    return
                exports = stored[1]
                state.stable_ticket_snapshots_seen.append(snapshot)
            if session_id not in exports:
                self.send_json({"error": "missing"}, 404)
                return
            if state.direct_ticket_url is not None and direct:
                url = state.direct_ticket_url
            elif state.cross_origin_ticket:
                url = "http://example.invalid/archive-api/v1/exports/private-ticket"
            elif direct:
                capability = hashlib.sha256(
                    f"{session_id}\0{snapshot}".encode()
                ).hexdigest()
                state.direct_ticket_targets[capability] = (session_id, snapshot)
                url = "/archive-api/v1/exports/" + capability
            else:
                url = "/archive-api/v1/exports/ticket-" + urllib.parse.quote(
                    session_id, safe=""
                )
                if snapshot:
                    url += "?snapshot=" + urllib.parse.quote(snapshot, safe="")
            response: dict[str, Any] = {"url": url}
            if snapshot:
                response.update(
                    {
                        "cursor_protocol": STABLE_CURSOR_PROTOCOL,
                        "snapshot": snapshot,
                        "records_sha256": records_digest(exports[session_id]),
                    }
                )
            self.send_management_json(response)
            return
        ticket_prefix = "/archive-api/v1/exports/ticket-"
        direct_ticket_prefix = "/archive-api/v1/exports/"
        if parsed.path.startswith(direct_ticket_prefix) and not parsed.path.startswith(ticket_prefix):
            state.ticket_calls += 1
            if self.headers.get("Authorization"):
                state.ticket_authorization_seen = True
            if state.expire_direct_ticket_once:
                state.expire_direct_ticket_once = False
                self.send_json({"error": "expired"}, 404)
                return
            capability = parsed.path[len(direct_ticket_prefix) :]
            target = state.direct_ticket_targets.get(capability)
            if target is None:
                self.send_json({"error": "ticket"}, 404)
                return
            session_id, snapshot = target
            stored = state.stable_snapshots.get(snapshot)
            rows = None if stored is None else stored[1].get(session_id)
            if rows is None:
                self.send_json({"error": "ticket"}, 404)
                return
            body = b"\n" * state.ticket_whitespace_bytes + b"".join(
                json.dumps(row, separators=(",", ":")).encode("utf-8") + b"\n"
                for row in rows
            )
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        if parsed.path.startswith(ticket_prefix):
            state.ticket_calls += 1
            if self.headers.get("Authorization"):
                state.ticket_authorization_seen = True
            session_id = urllib.parse.unquote(parsed.path[len(ticket_prefix) :])
            query = urllib.parse.parse_qs(parsed.query)
            snapshot = query.get("snapshot", [""])[0]
            exports = state.exports
            if snapshot:
                stored = state.stable_snapshots.get(snapshot)
                if not state.serve_live_snapshot_exports:
                    exports = {} if stored is None else stored[1]
            rows = exports.get(session_id)
            if rows is None:
                self.send_json({"error": "ticket"}, 404)
                return
            body = b"\n" * state.ticket_whitespace_bytes + b"".join(
                json.dumps(row, separators=(",", ":")).encode("utf-8") + b"\n"
                for row in rows
            )
            self.send_response(200)
            self.send_header("Content-Type", "application/x-ndjson")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            return
        self.send_json({"error": "not found"}, 404)


class SourceServer(http.server.ThreadingHTTPServer):
    def __init__(self, state: SourceState) -> None:
        super().__init__(("127.0.0.1", 0), SourceHandler)
        self.state = state


class DeltaExporterTest(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = Path(tempfile.mkdtemp(prefix="mtc-delta-export-test."))
        os.chmod(self.temporary, 0o700)
        self.token_file = self.temporary / "token"
        self.token_file.write_text(TOKEN + "\n", encoding="utf-8")
        os.chmod(self.token_file, 0o600)
        self.checkpoint = self.temporary / "checkpoint.json"
        self.state = SourceState()
        self.server = SourceServer(self.state)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        host, port = self.server.server_address
        self.base_url = f"http://{host}:{port}"

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)
        shutil.rmtree(self.temporary)

    def run_export(
        self,
        output: Path,
        *,
        since: str | None = "2026-08-16T00:00:00Z",
        token_env: bool = False,
        extra: list[str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            "python3",
            str(EXPORTER),
            "--base-url",
            self.base_url,
            "--allow-http",
            "--checkpoint",
            str(self.checkpoint),
            "--output",
            str(output),
            "--overlap-seconds",
            "3600",
            "--max-line-bytes",
            "1024",
            "--max-download-bytes",
            "1048576",
            "--max-output-bytes",
            "1048576",
            "--timeout-seconds",
            "5",
        ]
        environment = os.environ.copy()
        if token_env:
            command.extend(["--token-env", "MTC_TEST_SOURCE_TOKEN"])
            environment["MTC_TEST_SOURCE_TOKEN"] = TOKEN
        else:
            command.extend(["--token-file", str(self.token_file)])
        if since is not None:
            command.extend(["--since", since])
        if extra:
            command.extend(extra)
        return subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            text=True,
            capture_output=True,
            timeout=15,
            check=False,
        )

    def run_direct_export(
        self,
        output: Path,
        *,
        since: str | None = "2026-08-16T00:00:00Z",
        offline_full: bool = True,
        hostile_proxy: bool = False,
        extra: list[str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            "python3",
            str(EXPORTER),
            "--collector-direct",
            "--base-url",
            self.base_url,
            "--private-http-host",
            "127.0.0.1",
            "--checkpoint",
            str(self.checkpoint),
            "--output",
            str(output),
            "--overlap-seconds",
            "3600",
            "--max-line-bytes",
            "1024",
            "--max-download-bytes",
            "1048576",
            "--max-output-bytes",
            "1048576",
            "--timeout-seconds",
            "5",
            "--max-elapsed-seconds",
            "15",
            "--retry-base-seconds",
            "0.01",
        ]
        if offline_full:
            command.append("--offline-full")
        if since is not None:
            command.extend(["--since", since])
        if extra:
            command.extend(extra)
        environment = os.environ.copy()
        if hostile_proxy:
            environment.update(
                {
                    "HTTP_PROXY": "http://127.0.0.1:9",
                    "HTTPS_PROXY": "http://127.0.0.1:9",
                    "ALL_PROXY": "http://127.0.0.1:9",
                    "NO_PROXY": "",
                }
            )
        return subprocess.run(
            command,
            cwd=ROOT,
            env=environment,
            text=True,
            capture_output=True,
            timeout=20,
            check=False,
        )

    def assert_private_file(self, path: Path) -> None:
        mode = stat.S_IMODE(path.stat().st_mode)
        self.assertEqual(mode & 0o077, 0, f"unsafe mode {mode:o} for {path}")

    def configure_two_records(self) -> tuple[dict[str, Any], dict[str, Any]]:
        first = record(
            "request-1",
            "private-session-one",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        second = record(
            "request-2",
            "private-session-two",
            "2026-08-16T02:00:00Z",
            "2026-08-16T02:01:00Z",
        )
        self.state.exports = {
            first["session_id"]: [first],
            second["session_id"]: [second],
        }
        self.state.sessions = [
            summary(second["session_id"], [second]),
            summary(first["session_id"], [first]),
        ]
        self.state.stats_values = [2, 2]
        return first, second

    def test_exports_canonical_deterministic_private_delta(self) -> None:
        first, second = self.configure_two_records()
        self.state.plugin_envelope = True
        output = self.temporary / "delta-1.jsonl"
        result = self.run_export(output)
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = [json.loads(line) for line in output.read_text().splitlines()]
        self.assertEqual([row["request_id"] for row in exported], ["request-1", "request-2"])
        self.assertEqual(exported, [first, second])
        manifest_path = Path(str(output) + ".manifest.json")
        manifest = json.loads(manifest_path.read_text())
        checkpoint = json.loads(self.checkpoint.read_text())
        self.assertEqual(manifest["sequence"], 1)
        self.assertEqual(manifest["record_count"], 2)
        self.assertEqual(
            manifest["prior_watermark_completed_at"], "2026-08-16T00:00:00.000000Z"
        )
        self.assertEqual(checkpoint["watermark_completed_at"], manifest["watermark_completed_at"])
        self.assertFalse(self.state.ticket_authorization_seen)
        for path in (output, manifest_path, self.checkpoint):
            self.assert_private_file(path)
        combined = result.stdout + result.stderr
        for secret in (
            TOKEN,
            "private-session-one",
            "private-session-two",
            "ticket-private",
            "payload-secret-that-must-never-appear",
        ):
            self.assertNotIn(secret, combined)

    def test_legacy_plugin_input_accepts_named_environment_secret(self) -> None:
        self.configure_two_records()
        output = self.temporary / "legacy-env-token.jsonl"
        result = self.run_export(output, token_env=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)

    def test_incremental_overlap_and_pending_resume_are_replay_safe(self) -> None:
        first = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.exports = {first["session_id"]: [first]}
        self.state.sessions = [summary(first["session_id"], [first])]
        self.state.stats_values = [1, 1]
        output_one = self.temporary / "delta-1.jsonl"
        result = self.run_export(output_one)
        self.assertEqual(result.returncode, 0, result.stderr)
        first_checkpoint = self.checkpoint.read_bytes()

        second = record(
            "request-2",
            "private-session",
            "2026-08-16T02:00:00Z",
            "2026-08-16T02:01:00Z",
        )
        self.state.exports = {first["session_id"]: [first, second]}
        self.state.sessions = [summary(first["session_id"], [first, second])]
        self.state.stats_values = [2, 2]
        output_two = self.temporary / "delta-2.jsonl"
        result = self.run_export(output_two, since=None)
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = [json.loads(line) for line in output_two.read_text().splitlines()]
        self.assertEqual([row["request_id"] for row in exported], ["request-1", "request-2"])

        # Model a crash after manifest durability but before final rename/checkpoint.
        self.checkpoint.write_bytes(first_checkpoint)
        pending = Path(str(output_two) + ".pending")
        output_two.replace(pending)
        calls_before_resume = self.state.request_count
        result = self.run_export(output_two, since=None, extra=["--resume"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(output_two.is_file())
        self.assertFalse(pending.exists())
        self.assertEqual(self.state.request_count, calls_before_resume)
        checkpoint = json.loads(self.checkpoint.read_text())
        self.assertEqual(checkpoint["sequence"], 2)

        # A second resume is idempotent but still verifies the sealed output.
        result = self.run_export(output_two, since=None, extra=["--resume"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.state.request_count, calls_before_resume)

        sealed_checkpoint = self.checkpoint.read_bytes()
        with output_two.open("ab") as stream:
            stream.write(b"{}\n")
        result = self.run_export(output_two, since=None, extra=["--resume"])
        self.assertEqual(result.returncode, 2)
        self.assertIn("does not match its manifest", result.stderr)
        self.assertEqual(self.checkpoint.read_bytes(), sealed_checkpoint)

    def test_resume_reexports_an_unsealed_orphan_pending_file(self) -> None:
        row = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.exports = {row["session_id"]: [row]}
        self.state.sessions = [summary(row["session_id"], [row])]
        self.state.stats_values = [1, 1]
        output = self.temporary / "orphan-recovery.jsonl"
        pending = Path(str(output) + ".pending")
        pending.write_bytes(b"unsealed partial output")
        os.chmod(pending, 0o600)
        result = self.run_export(output, extra=["--resume"])
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertTrue(output.is_file())
        self.assertFalse(pending.exists())
        self.assertTrue(self.checkpoint.is_file())

    def test_completed_at_inside_overlap_retains_an_earlier_started_record(self) -> None:
        old = record(
            "request-old",
            "private-session",
            "2026-08-15T22:00:00Z",
            "2026-08-15T22:10:00Z",
        )
        boundary = record(
            "request-boundary",
            "private-session",
            "2026-08-15T23:50:00Z",
            "2026-08-16T00:01:00Z",
        )
        self.state.exports = {old["session_id"]: [old, boundary]}
        self.state.sessions = [summary(old["session_id"], [old, boundary])]
        self.state.stats_values = [2, 2]
        output = self.temporary / "completed-boundary.jsonl"
        result = self.run_export(output, since="2026-08-16T01:00:00Z")
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = [json.loads(line) for line in output.read_text().splitlines()]
        self.assertEqual(
            [item["request_id"] for item in exported], ["request-boundary"]
        )

    def test_refuses_projection_drift_without_advancing_checkpoint(self) -> None:
        row = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        initial = summary(row["session_id"], [row])
        changed = dict(initial)
        changed["last_at"] = "2026-08-16T01:02:00Z"
        self.state.exports = {row["session_id"]: [row]}
        self.state.sessions = [initial]
        self.state.session_snapshots = [[initial], [changed]]
        self.state.stats_values = [1, 1]
        output = self.temporary / "drift.jsonl"
        result = self.run_export(output)
        self.assertEqual(result.returncode, 2)
        self.assertIn("projection changed", result.stderr)
        self.assertNotIn(row["session_id"], result.stderr)
        self.assertFalse(output.exists())
        self.assertFalse(self.checkpoint.exists())

    def test_coalesces_identical_replay_lines_but_refuses_conflicts(self) -> None:
        row = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.exports = {row["session_id"]: [row, row]}
        self.state.sessions = [summary(row["session_id"], [row])]
        self.state.stats_values = [1, 1]
        output = self.temporary / "duplicate.jsonl"
        result = self.run_export(output)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(len(output.read_text().splitlines()), 1)

        conflicting = dict(row)
        conflicting["response"] = {"answer": "different"}
        self.state.exports = {row["session_id"]: [row, conflicting]}
        self.state.session_calls = 0
        self.state.stats_calls = 0
        self.state.export_calls = 0
        self.state.ticket_calls = 0
        other_checkpoint = self.temporary / "other-checkpoint.json"
        original_checkpoint = self.checkpoint
        self.checkpoint = other_checkpoint
        try:
            result = self.run_export(self.temporary / "conflict.jsonl")
        finally:
            self.checkpoint = original_checkpoint
        self.assertEqual(result.returncode, 2)
        self.assertIn("conflicting archive records", result.stderr)
        self.assertNotIn(row["session_id"], result.stderr)
        self.assertFalse(other_checkpoint.exists())

    def test_refuses_saturated_window_and_session_count_mismatch(self) -> None:
        first, second = self.configure_two_records()
        output = self.temporary / "saturated.jsonl"
        result = self.run_export(output, extra=["--session-limit", "2"])
        self.assertEqual(result.returncode, 2)
        self.assertIn(STABLE_CURSOR_PROTOCOL, result.stderr)
        self.assertEqual(self.state.export_calls, 0)
        self.assertFalse(self.checkpoint.exists())

        self.state.sessions = [summary(first["session_id"], [first, second])]
        self.state.exports = {first["session_id"]: [first]}
        self.state.stats_values = [2, 2]
        mismatch = self.temporary / "mismatch.jsonl"
        result = self.run_export(mismatch)
        self.assertEqual(result.returncode, 2)
        self.assertIn("count disagrees", result.stderr)
        self.assertFalse(self.checkpoint.exists())

    def test_stable_snapshot_exports_across_pages_and_defers_concurrent_addition(self) -> None:
        rows = [
            record(
                f"request-{index}",
                f"session-{index}",
                f"2026-08-16T0{index}:00:00Z",
                f"2026-08-16T0{index}:01:00Z",
            )
            for index in range(1, 4)
        ]
        self.state.stable_cursor_supported = True
        self.state.exports = {row["session_id"]: [row] for row in rows}
        self.state.sessions = sorted(
            (summary(row["session_id"], [row]) for row in rows),
            key=lambda item: item["last_at"],
            reverse=True,
        )
        concurrent = record(
            "request-4",
            "session-4",
            "2026-08-15T20:00:00Z",
            "2026-08-15T20:01:00Z",
        )
        self.state.after_snapshot_sessions = [summary("session-4", [concurrent])]
        self.state.after_snapshot_exports = {"session-4": [concurrent]}

        output = self.temporary / "stable-page-delta.jsonl"
        result = self.run_export(output, extra=["--session-limit", "2"])
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = [json.loads(line) for line in output.read_text().splitlines()]
        self.assertEqual(
            [item["request_id"] for item in exported],
            ["request-1", "request-2", "request-3"],
        )
        manifest = json.loads(Path(str(output) + ".manifest.json").read_text())
        self.assertEqual(
            manifest["session_projection_protocol"], STABLE_CURSOR_PROTOCOL
        )
        self.assertEqual(manifest["version"], 2)
        self.assertIsNone(manifest["prior_source_ingest_fence"])
        self.assertEqual(manifest["source_ingest_fence"], "3")
        self.assertEqual(manifest["session_count"], 3)
        self.assertEqual(manifest["source_projection_requests"], 3)
        self.assertRegex(manifest["source_snapshot_sha256"], r"^[0-9a-f]{64}$")
        self.assertEqual(len(self.state.stable_ticket_snapshots_seen), 3)
        self.assertEqual(len(set(self.state.stable_ticket_snapshots_seen)), 1)

        calls_before_resume = self.state.request_count
        result = self.run_export(
            output, extra=["--session-limit", "2", "--resume"]
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.state.request_count, calls_before_resume)

        replay = self.temporary / "stable-page-replay.jsonl"
        result = self.run_export(replay, since=None, extra=["--session-limit", "2"])
        self.assertEqual(result.returncode, 0, result.stderr)
        replayed = [json.loads(line) for line in replay.read_text().splitlines()]
        self.assertEqual(
            [item["request_id"] for item in replayed],
            ["request-4", "request-2", "request-3"],
        )
        replay_manifest = json.loads(
            Path(str(replay) + ".manifest.json").read_text()
        )
        self.assertEqual(replay_manifest["prior_source_ingest_fence"], "3")
        self.assertEqual(replay_manifest["source_ingest_fence"], "4")

    def test_stable_fence_captures_old_record_appended_to_existing_session(self) -> None:
        old = record(
            "request-old",
            "session-old",
            "2026-08-10T01:00:00Z",
            "2026-08-10T01:01:00Z",
        )
        recent = record(
            "request-recent",
            "session-recent",
            "2026-08-20T01:00:00Z",
            "2026-08-20T01:01:00Z",
        )
        late = record(
            "request-late",
            "session-old",
            "2026-08-11T01:00:00Z",
            "2026-08-11T01:01:00Z",
        )
        self.state.stable_cursor_supported = True
        self.state.sessions = [
            summary("session-recent", [recent]),
            summary("session-old", [old]),
        ]
        self.state.exports = {
            "session-recent": [recent],
            "session-old": [old],
        }
        self.state.after_snapshot_sessions = [summary("session-old", [old, late])]
        self.state.after_snapshot_exports = {"session-old": [old, late]}

        first_output = self.temporary / "existing-session-first.jsonl"
        result = self.run_export(first_output, since="2026-08-01T00:00:00Z")
        self.assertEqual(result.returncode, 0, result.stderr)

        second_output = self.temporary / "existing-session-second.jsonl"
        result = self.run_export(second_output, since=None)
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = [
            json.loads(line) for line in second_output.read_text().splitlines()
        ]
        self.assertEqual(
            [item["request_id"] for item in exported],
            ["request-old", "request-late", "request-recent"],
        )
        manifest = json.loads(
            Path(str(second_output) + ".manifest.json").read_text()
        )
        self.assertEqual(manifest["prior_source_ingest_fence"], "2")
        self.assertEqual(manifest["source_ingest_fence"], "3")

    def test_stable_cursor_handles_more_than_one_thousand_equal_timestamps(self) -> None:
        self.state.stable_cursor_supported = True
        shared_time = "2026-08-16T01:00:00Z"
        self.state.sessions = [
            {
                "session_id": f"session-{index:04d}",
                "requests": 0,
                "first_at": shared_time,
                "last_at": shared_time,
            }
            for index in range(1001)
        ]
        client = DELTA_EXPORT.SourceClient(
            self.base_url,
            self.base_url,
            TOKEN,
            5,
            True,
        )
        projection = client.stable_sessions(
            1000,
            dt.datetime(2026, 8, 16, tzinfo=dt.timezone.utc),
        )
        self.assertEqual(len(projection.sessions), 1001)
        self.assertEqual(
            [item["session_id"] for item in projection.sessions[:2]],
            ["session-0000", "session-0001"],
        )
        replay = client.stable_sessions(
            1000,
            dt.datetime(2026, 8, 16, tzinfo=dt.timezone.utc),
            projection.snapshot,
        )
        self.assertEqual(
            session_set_digest(replay.sessions), session_set_digest(projection.sessions)
        )

    def test_stable_cursor_refuses_page_gap_and_cursor_loop(self) -> None:
        self.state.stable_cursor_supported = True
        shared_time = "2026-08-16T01:00:00Z"
        self.state.sessions = [
            {
                "session_id": f"session-{index}",
                "requests": 0,
                "first_at": shared_time,
                "last_at": shared_time,
            }
            for index in range(5)
        ]
        client = DELTA_EXPORT.SourceClient(
            self.base_url,
            self.base_url,
            TOKEN,
            5,
            True,
        )
        self.state.stable_gap_offset = 2
        with self.assertRaisesRegex(DELTA_EXPORT.DeltaError, "gap"):
            client.stable_sessions(
                2,
                dt.datetime(2026, 8, 16, tzinfo=dt.timezone.utc),
            )

        self.state.stable_gap_offset = None
        self.state.stable_cursor_loop = True
        with self.assertRaisesRegex(DELTA_EXPORT.DeltaError, "cursor loop"):
            client.stable_sessions(
                2,
                dt.datetime(2026, 8, 16, tzinfo=dt.timezone.utc),
            )

    def test_stable_snapshot_refuses_equal_count_live_export_substitution(self) -> None:
        original = record(
            "request-original",
            "session-one",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        replacement = record(
            "request-replacement",
            "session-one",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:02:00Z",
        )
        self.state.stable_cursor_supported = True
        self.state.sessions = [summary("session-one", [original])]
        self.state.exports = {"session-one": [original]}
        self.state.after_snapshot_exports = {"session-one": [replacement]}
        self.state.serve_live_snapshot_exports = True
        result = self.run_export(
            self.temporary / "live-substitution.jsonl",
            extra=["--session-limit", "1"],
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("digest disagrees", result.stderr)
        self.assertFalse(self.checkpoint.exists())

    def test_final_stats_read_closes_stable_source_write_barrier(self) -> None:
        row = record(
            "request-1",
            "session-one",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.stable_cursor_supported = True
        self.state.sessions = [summary("session-one", [row])]
        self.state.exports = {"session-one": [row]}
        self.state.stats_values = [1, 1, 2]
        result = self.run_export(
            self.temporary / "late-write-barrier.jsonl",
            extra=["--session-limit", "1", "--require-stable-source"],
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("write barrier", result.stderr)
        self.assertFalse(self.checkpoint.exists())

    def test_refuses_redirect_cross_origin_ticket_and_unstable_freeze(self) -> None:
        row = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.exports = {row["session_id"]: [row]}
        self.state.sessions = [summary(row["session_id"], [row])]
        self.state.stats_values = [1, 1]

        self.state.redirect_sessions = True
        result = self.run_export(self.temporary / "redirect.jsonl")
        self.assertEqual(result.returncode, 2)
        self.assertEqual(self.state.leak_calls, 0)
        self.assertNotIn(TOKEN, result.stderr)

        self.state.redirect_sessions = False
        self.state.cross_origin_ticket = True
        result = self.run_export(self.temporary / "cross-origin.jsonl")
        self.assertEqual(result.returncode, 2)
        self.assertIn("escaped", result.stderr)
        self.assertFalse(self.checkpoint.exists())

        self.state.cross_origin_ticket = False
        self.state.stats_calls = 0
        self.state.stats_values = [1, 2]
        result = self.run_export(
            self.temporary / "unstable.jsonl", extra=["--require-stable-source"]
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("write barrier", result.stderr)
        self.assertFalse(self.checkpoint.exists())

    def test_collector_direct_offline_full_retries_transients_and_expired_ticket(self) -> None:
        first, second = self.configure_two_records()
        self.state.stable_cursor_supported = True
        self.state.offline_full_enabled = True
        self.state.stable_transient_statuses = [503] * 7 + [429]
        self.state.expire_direct_ticket_once = True
        output = self.temporary / "collector-full.jsonl"
        result = self.run_direct_export(
            output,
            hostile_proxy=True,
            extra=["--session-limit", "1", "--max-retries", "1"],
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        exported = [json.loads(line) for line in output.read_text().splitlines()]
        self.assertEqual(exported, [first, second])
        manifest = json.loads(Path(str(output) + ".manifest.json").read_text())
        self.assertEqual(manifest["source_mode"], "collector-direct")
        self.assertTrue(manifest["offline_full_snapshot"])
        self.assertEqual(manifest["session_projection_protocol"], STABLE_CURSOR_PROTOCOL)
        self.assertFalse(self.state.direct_authorization_seen)
        self.assertFalse(self.state.ticket_authorization_seen)
        self.assertGreaterEqual(self.state.ticket_calls, 3)
        calls_before_resume = self.state.request_count
        replay = self.run_direct_export(
            output,
            extra=["--session-limit", "1", "--resume"],
        )
        self.assertEqual(replay.returncode, 0, replay.stderr)
        self.assertEqual(self.state.request_count, calls_before_resume)

    def test_collector_direct_requires_offline_advertisement_and_never_falls_back(self) -> None:
        self.configure_two_records()
        self.state.stable_cursor_supported = True
        missing_flag = self.run_direct_export(
            self.temporary / "missing-offline.jsonl", offline_full=False
        )
        self.assertEqual(missing_flag.returncode, 2)
        self.assertIn("requires --offline-full", missing_flag.stderr)

        not_advertised = self.run_direct_export(
            self.temporary / "not-advertised.jsonl"
        )
        self.assertEqual(not_advertised.returncode, 2)
        self.assertIn("does not advertise", not_advertised.stderr)

        self.state.offline_full_enabled = True
        self.state.stable_cursor_supported = False
        no_stable = self.run_direct_export(self.temporary / "no-stable.jsonl")
        self.assertEqual(no_stable.returncode, 2)
        self.assertIn("does not implement", no_stable.stderr)
        self.assertFalse(self.checkpoint.exists())

    def test_collector_direct_incremental_fence_captures_old_timestamp_arrival(self) -> None:
        original = record(
            "request-original",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.sessions = [summary(original["session_id"], [original])]
        self.state.exports = {original["session_id"]: [original]}
        self.state.stats_values = [1, 1, 1, 1]
        self.state.stable_cursor_supported = True
        self.state.offline_full_enabled = True
        first_output = self.temporary / "collector-baseline.jsonl"
        first_result = self.run_direct_export(first_output)
        self.assertEqual(first_result.returncode, 0, first_result.stderr)
        first_checkpoint = json.loads(self.checkpoint.read_text())

        late = record(
            "request-late",
            "private-session",
            "2026-08-15T01:00:00Z",
            "2026-08-15T01:01:00Z",
        )
        self.state.exports[original["session_id"]].append(late)
        self.state.sessions = [summary(original["session_id"], [original, late])]
        self.state.stats_values = [2, 2, 2]
        second_output = self.temporary / "collector-delta.jsonl"
        second_result = self.run_direct_export(
            second_output,
            since=None,
            offline_full=False,
        )
        self.assertEqual(second_result.returncode, 0, second_result.stderr)
        exported = [json.loads(line) for line in second_output.read_text().splitlines()]
        self.assertEqual(
            {item["request_id"] for item in exported},
            {"request-original", "request-late"},
        )
        second_manifest = json.loads(
            Path(str(second_output) + ".manifest.json").read_text()
        )
        self.assertEqual(
            second_manifest["prior_source_ingest_fence"],
            first_checkpoint["source_ingest_fence"],
        )
        self.assertGreater(
            int(second_manifest["source_ingest_fence"]),
            int(second_manifest["prior_source_ingest_fence"]),
        )

    def test_collector_direct_rejects_malicious_ticket_and_cpa_token(self) -> None:
        row = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.sessions = [summary(row["session_id"], [row])]
        self.state.exports = {row["session_id"]: [row]}
        self.state.stats_values = [1, 1]
        self.state.stable_cursor_supported = True
        self.state.offline_full_enabled = True
        malicious = [
            "//example.invalid/archive-api/v1/exports/" + "a" * 64,
            "/archive-api/v1/exports/" + "a" * 64 + "?leak=1",
            "/archive-api/v1/exports/%2e%2e",
            "/archive-api/v1/exports/not-a-capability",
        ]
        for index, value in enumerate(malicious):
            self.state.direct_ticket_url = value
            result = self.run_direct_export(
                self.temporary / f"malicious-{index}.jsonl",
                extra=["--max-retries", "0"],
            )
            self.assertEqual(result.returncode, 2, value)
            self.assertFalse(self.checkpoint.exists())
        self.state.direct_ticket_url = None
        result = self.run_direct_export(
            self.temporary / "token-refused.jsonl",
            extra=["--token-env", "MTC_TEST_SOURCE_TOKEN"],
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("does not accept a CPA token", result.stderr)
        self.assertNotIn(TOKEN, result.stdout + result.stderr)

    def test_collector_direct_410_discards_attempt_without_checkpoint(self) -> None:
        self.configure_two_records()
        self.state.stable_cursor_supported = True
        self.state.offline_full_enabled = True
        self.state.expire_stable_replay_once = True
        output = self.temporary / "expired-snapshot.jsonl"
        result = self.run_direct_export(
            output,
            extra=["--max-retries", "0"],
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("repeatedly expired", result.stderr)
        self.assertFalse(output.exists())
        self.assertFalse(Path(str(output) + ".manifest.json").exists())
        self.assertFalse(self.checkpoint.exists())

    def test_legacy_source_fingerprint_remains_v1_compatible(self) -> None:
        client = DELTA_EXPORT.SourceClient(
            self.base_url,
            self.base_url,
            TOKEN,
            5,
            True,
        )
        material = json.dumps(
            {
                "origin": self.base_url,
                "base": self.base_url,
                "download_origin": self.base_url,
                "download_base": self.base_url,
                "sessions_path": "/v0/management/plugins/cpa-session-archive/sessions",
                "export_path": "/v0/management/plugins/cpa-session-archive/export",
                "stats_path": "/v0/management/plugins/cpa-session-archive/stats",
                "version": 1,
            },
            sort_keys=True,
            separators=(",", ":"),
        ).encode()
        self.assertEqual(
            DELTA_EXPORT.source_fingerprint(client),
            hashlib.sha256(material).hexdigest(),
        )
        self.assertEqual(client._retry_delay(1_000_000, None), 10.0)

    def test_blank_download_bytes_are_bounded_before_json_filtering(self) -> None:
        row = record(
            "request-1",
            "private-session",
            "2026-08-16T01:00:00Z",
            "2026-08-16T01:01:00Z",
        )
        self.state.sessions = [summary(row["session_id"], [row])]
        self.state.exports = {row["session_id"]: [row]}
        self.state.stats_values = [1, 1]
        self.state.ticket_whitespace_bytes = 2048
        result = self.run_export(
            self.temporary / "blank-flood.jsonl",
            extra=["--max-download-bytes", "1024"],
        )
        self.assertEqual(result.returncode, 2)
        self.assertIn("downloads exceed", result.stderr)
        self.assertFalse(self.checkpoint.exists())

    def test_checkpoint_lock_bounds_concurrent_export_before_source_access(self) -> None:
        lock_path = DELTA_EXPORT.checkpoint_lock_path(self.checkpoint)
        descriptor = os.open(lock_path, os.O_RDWR | os.O_CREAT, 0o600)
        try:
            fcntl.flock(descriptor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            result = self.run_export(
                self.temporary / "locked.jsonl",
                extra=["--max-elapsed-seconds", "0.05"],
            )
        finally:
            fcntl.flock(descriptor, fcntl.LOCK_UN)
            os.close(descriptor)
        self.assertEqual(result.returncode, 2)
        self.assertIn("transaction lock", result.stderr)
        self.assertEqual(self.state.request_count, 0)
        self.assertFalse(self.checkpoint.exists())

    def test_source_record_count_cannot_move_backwards_across_checkpoint(self) -> None:
        self.configure_two_records()
        first = self.run_export(self.temporary / "count-before.jsonl")
        self.assertEqual(first.returncode, 0, first.stderr)
        checkpoint_before = self.checkpoint.read_bytes()
        self.state.stats_calls = 0
        self.state.stats_values = [1]
        second = self.run_export(
            self.temporary / "count-after.jsonl",
            since=None,
        )
        self.assertEqual(second.returncode, 2)
        self.assertIn("moved backwards", second.stderr)
        self.assertEqual(self.checkpoint.read_bytes(), checkpoint_before)


if __name__ == "__main__":
    unittest.main(verbosity=2)
