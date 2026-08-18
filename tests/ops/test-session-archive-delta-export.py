#!/usr/bin/env python3
"""Black-box tests for the CPA/API2 session archive delta exporter."""

from __future__ import annotations

import base64
import http.server
import json
import os
import shutil
import stat
import subprocess
import tempfile
import threading
import unittest
import urllib.parse
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[2]
EXPORTER = ROOT / "ops" / "export-cpa-session-archive-delta.py"
TOKEN = "management-token-that-must-never-appear"


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


class SourceState:
    def __init__(self) -> None:
        self.sessions: list[dict[str, Any]] = []
        self.session_snapshots: list[list[dict[str, Any]]] = []
        self.exports: dict[str, list[dict[str, Any]]] = {}
        self.stats_values: list[int] = []
        self.session_calls = 0
        self.stats_calls = 0
        self.export_calls = 0
        self.ticket_calls = 0
        self.leak_calls = 0
        self.ticket_authorization_seen = False
        self.redirect_sessions = False
        self.cross_origin_ticket = False
        self.plugin_envelope = False

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
        if parsed.path == "/leak":
            state.leak_calls += 1
            self.send_json({"authorization": self.headers.get("Authorization")})
            return
        if parsed.path == sessions_path:
            state.session_calls += 1
            if not self.management_authorized():
                return
            if state.redirect_sessions:
                self.send_response(302)
                self.send_header("Location", "/leak")
                self.end_headers()
                return
            snapshots = state.session_snapshots or [state.sessions]
            index = min(state.session_calls - 1, len(snapshots) - 1)
            self.send_management_json({"sessions": snapshots[index]})
            return
        if parsed.path == stats_path:
            state.stats_calls += 1
            if not self.management_authorized():
                return
            default_count = sum(len(rows) for rows in state.exports.values())
            values = state.stats_values or [default_count]
            index = min(state.stats_calls - 1, len(values) - 1)
            self.send_management_json(
                {"records": values[index], "sessions": len(state.sessions)}
            )
            return
        if parsed.path == export_path:
            state.export_calls += 1
            if not self.management_authorized():
                return
            query = urllib.parse.parse_qs(parsed.query)
            session_id = query.get("id", [""])[0]
            if session_id not in state.exports:
                self.send_json({"error": "missing"}, 404)
                return
            if state.cross_origin_ticket:
                url = "http://example.invalid/archive-api/v1/exports/private-ticket"
            else:
                url = "/archive-api/v1/exports/ticket-" + urllib.parse.quote(
                    session_id, safe=""
                )
            self.send_management_json({"url": url})
            return
        ticket_prefix = "/archive-api/v1/exports/ticket-"
        if parsed.path.startswith(ticket_prefix):
            state.ticket_calls += 1
            if self.headers.get("Authorization"):
                state.ticket_authorization_seen = True
            session_id = urllib.parse.unquote(parsed.path[len(ticket_prefix) :])
            rows = state.exports.get(session_id)
            if rows is None:
                self.send_json({"error": "ticket"}, 404)
                return
            body = b"".join(
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
        extra: list[str] | None = None,
    ) -> subprocess.CompletedProcess[str]:
        command = [
            "python3",
            str(EXPORTER),
            "--base-url",
            self.base_url,
            "--allow-http",
            "--token-file",
            str(self.token_file),
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
        if since is not None:
            command.extend(["--since", since])
        if extra:
            command.extend(extra)
        return subprocess.run(
            command,
            cwd=ROOT,
            text=True,
            capture_output=True,
            timeout=15,
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
        self.assertIn("completeness is unprovable", result.stderr)
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


if __name__ == "__main__":
    unittest.main(verbosity=2)
