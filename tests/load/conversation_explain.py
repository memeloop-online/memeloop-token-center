#!/usr/bin/env python3
"""Read-only PostgreSQL EXPLAIN gate for imported-scale conversation paging."""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import Any


SAFE_UUID = re.compile(r"^[0-9A-Fa-f-]{36}$")


class GateFailure(RuntimeError):
    pass


def psql(database_url: str, sql: str, timeout_ms: int) -> str:
    environment = os.environ.copy()
    environment["PGDATABASE"] = database_url
    environment["PGOPTIONS"] = (
        f"{environment.get('PGOPTIONS', '')} -c default_transaction_read_only=on "
        f"-c statement_timeout={timeout_ms}"
    ).strip()
    result = subprocess.run(
        ["psql", "-X", "-q", "-A", "-t", "--no-psqlrc"],
        input=sql,
        capture_output=True,
        text=True,
        env=environment,
        timeout=max(30, timeout_ms // 1000 + 10),
        check=False,
    )
    if result.returncode:
        raise GateFailure((result.stderr.strip().splitlines() or ["psql failed"])[-1])
    return result.stdout.strip()


def scalar(database_url: str, sql: str, timeout_ms: int) -> str:
    rows = psql(database_url, sql, timeout_ms).splitlines()
    return rows[-1].strip() if rows else ""


def nodes(plan: dict[str, Any]) -> list[dict[str, Any]]:
    return [plan] + [item for child in plan.get("Plans", []) for item in nodes(child)]


def explain(database_url: str, name: str, query: str, timeout_ms: int) -> dict[str, Any]:
    document = json.loads(
        psql(
            database_url,
            f"EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) {query};",
            timeout_ms,
        )
    )[0]
    plan_nodes = nodes(document["Plan"])
    forbidden_scans = sorted(
        {
            str(node.get("Relation Name"))
            for node in plan_nodes
            if node.get("Node Type") == "Seq Scan"
            and str(node.get("Relation Name", "")).startswith(
                (
                    "request_records",
                    "conversation_key_clusters",
                    "conversation_observations",
                    "conversation_edges",
                )
            )
        }
    )
    return {
        "name": name,
        "execution_time_ms": document.get("Execution Time", 0.0),
        "returned_rows": document["Plan"].get("Actual Rows", 0),
        "indexes": sorted(
            {
                str(node["Index Name"])
                for node in plan_nodes
                if node.get("Index Name") is not None
            }
        ),
        "forbidden_sequential_scans": forbidden_scans,
        "plan": document["Plan"],
    }


def arguments() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--database-url-file", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--min-request-rows", type=int, default=110_000)
    parser.add_argument("--max-execution-ms", type=float, default=250.0)
    parser.add_argument("--statement-timeout-ms", type=int, default=30_000)
    return parser.parse_args()


def main() -> int:
    args = arguments()
    if shutil.which("psql") is None:
        print(json.dumps({"passed": False, "error": "psql is not installed"}), file=sys.stderr)
        return 3
    try:
        database_url = (
            args.database_url_file.read_text().strip()
            if args.database_url_file
            else os.environ.get("MTC_BENCH_DATABASE_URL", "").strip()
        )
        if not database_url:
            raise GateFailure("MTC_BENCH_DATABASE_URL or --database-url-file is required")
        request_rows = int(
            scalar(database_url, "SELECT COUNT(*) FROM request_records;", args.statement_timeout_ms)
        )
        projection_rows = int(
            scalar(
                database_url,
                "SELECT COUNT(*) FROM conversation_key_clusters;",
                args.statement_timeout_ms,
            )
        )
        sample = scalar(
            database_url,
            "SELECT key_id || '|' || cluster_id FROM conversation_key_clusters "
            "ORDER BY request_count DESC, updated_at DESC LIMIT 1;",
            args.statement_timeout_ms,
        ).split("|")
        if len(sample) != 2 or not all(SAFE_UUID.fullmatch(value) for value in sample):
            raise GateFailure("no valid conversation projection is available")
        key_id, cluster_id = sample
        cursor = scalar(
            database_url,
            "SELECT created_at::text || '|' || id FROM request_records "
            f"WHERE key_id = '{key_id}' AND conversation_cluster_id = '{cluster_id}' "
            "ORDER BY created_at DESC, id DESC OFFSET 99 LIMIT 1;",
            args.statement_timeout_ms,
        ).split("|")
        if len(cursor) != 2 or not cursor[0].isdigit() or not SAFE_UUID.fullmatch(cursor[1]):
            cursor = ["9223372036854775807", "ffffffff-ffff-ffff-ffff-ffffffffffff"]
        before_created_at, before_request_id = cursor
        queries = [
            (
                "conversation_list_first_page",
                "SELECT p.cluster_id, c.explicit_session_id, p.updated_at, p.request_count, "
                "p.candidate_edge_count FROM conversation_key_clusters p "
                "JOIN conversation_clusters c ON c.id = p.cluster_id "
                f"WHERE p.key_id = '{key_id}' "
                "ORDER BY p.updated_at DESC, p.cluster_id DESC LIMIT 50",
            ),
            (
                "conversation_detail_membership",
                "SELECT p.cluster_id, p.request_count FROM conversation_key_clusters p "
                f"WHERE p.key_id = '{key_id}' AND p.cluster_id = '{cluster_id}'",
            ),
            (
                "conversation_detail_first_page",
                "SELECT id, created_at, protocol, model, status_code FROM request_records "
                f"WHERE key_id = '{key_id}' AND conversation_cluster_id = '{cluster_id}' "
                "ORDER BY created_at DESC, id DESC LIMIT 201",
            ),
            (
                "conversation_detail_older_page",
                "SELECT id, created_at, protocol, model, status_code FROM request_records "
                f"WHERE key_id = '{key_id}' AND conversation_cluster_id = '{cluster_id}' "
                f"AND (created_at < {before_created_at} OR "
                f"(created_at = {before_created_at} AND id < '{before_request_id}')) "
                "ORDER BY created_at DESC, id DESC LIMIT 201",
            ),
        ]
        results = [explain(database_url, name, query, args.statement_timeout_ms) for name, query in queries]
        checks: list[dict[str, Any]] = [
            {
                "name": "imported request scale",
                "actual": request_rows,
                "expected_minimum": args.min_request_rows,
                "passed": request_rows >= args.min_request_rows,
            },
            {
                "name": "conversation projections populated",
                "actual": projection_rows,
                "expected_minimum": 1,
                "passed": projection_rows > 0,
            },
        ]
        for result in results:
            checks.extend(
                [
                    {
                        "name": f"{result['name']} latency",
                        "actual": result["execution_time_ms"],
                        "expected_maximum": args.max_execution_ms,
                        "passed": result["execution_time_ms"] <= args.max_execution_ms,
                    },
                    {
                        "name": f"{result['name']} indexed",
                        "actual": result["forbidden_sequential_scans"],
                        "expected": [],
                        "passed": not result["forbidden_sequential_scans"],
                    },
                ]
            )
        report = {
            "schema_version": 1,
            "dataset": {"request_rows": request_rows, "projection_rows": projection_rows},
            "sample": {"key_id": key_id, "cluster_id": cluster_id},
            "thresholds": {
                "min_request_rows": args.min_request_rows,
                "max_execution_ms": args.max_execution_ms,
            },
            "results": results,
            "checks": checks,
            "passed": all(check["passed"] for check in checks),
        }
        if args.output:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps({"passed": report["passed"], "checks": checks}, indent=2))
        return 0 if report["passed"] else 2
    except (OSError, ValueError, json.JSONDecodeError, subprocess.SubprocessError, GateFailure) as error:
        print(json.dumps({"passed": False, "error": str(error)}), file=sys.stderr)
        return 3


if __name__ == "__main__":
    raise SystemExit(main())
