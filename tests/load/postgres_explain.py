#!/usr/bin/env python3
"""Read-only PostgreSQL plan/latency benchmark for high-volume observability queries."""

from __future__ import annotations

import argparse
import datetime as dt
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import Any


SAFE_ID = re.compile(r"^[0-9A-Fa-f-]{36}$")


class PrerequisiteFailure(RuntimeError):
    pass


def run_psql(database_url: str, sql: str, timeout_ms: int) -> str:
    environment = os.environ.copy()
    options = environment.get("PGOPTIONS", "")
    environment["PGOPTIONS"] = (
        f"{options} -c default_transaction_read_only=on -c statement_timeout={timeout_ms}"
    ).strip()
    # Keep credentials out of argv/process listings. libpq accepts a connection
    # URI in PGDATABASE just as it does in the dbname parameter.
    environment["PGDATABASE"] = database_url
    result = subprocess.run(
        ["psql", "-X", "-q", "-A", "-t", "--no-psqlrc"],
        input=sql,
        capture_output=True,
        text=True,
        env=environment,
        timeout=max(30, timeout_ms / 1000 + 10),
        check=False,
    )
    if result.returncode != 0:
        error = result.stderr.strip().splitlines()[-1:] or ["unknown psql error"]
        raise PrerequisiteFailure(error[0])
    return result.stdout.strip()


def scalar(database_url: str, sql: str, timeout_ms: int) -> str:
    lines = run_psql(database_url, sql, timeout_ms).splitlines()
    return lines[-1].strip() if lines else ""


def literal(database_url: str, sql: str, timeout_ms: int) -> str | None:
    value = scalar(database_url, f"SELECT quote_literal(value) FROM ({sql}) q(value);", timeout_ms)
    return value if value else None


def plan_nodes(plan: dict[str, Any]) -> list[dict[str, Any]]:
    nodes = [plan]
    for child in plan.get("Plans", []):
        nodes.extend(plan_nodes(child))
    return nodes


def explain(database_url: str, name: str, query: str, timeout_ms: int) -> dict[str, Any]:
    raw = run_psql(
        database_url,
        f"EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF) {query};",
        timeout_ms,
    )
    document = json.loads(raw)[0]
    nodes = plan_nodes(document["Plan"])
    indexes = sorted(
        {str(node["Index Name"]) for node in nodes if node.get("Index Name") is not None}
    )
    sequential_large_relations = sorted(
        {
            str(node.get("Relation Name"))
            for node in nodes
            if node.get("Node Type") == "Seq Scan"
            and str(node.get("Relation Name", "")).startswith(
                ("request_records", "request_events")
            )
        }
    )
    return {
        "name": name,
        "planning_time_ms": document.get("Planning Time", 0.0),
        "execution_time_ms": document.get("Execution Time", 0.0),
        "returned_rows": document["Plan"].get("Actual Rows", 0),
        "root_node": document["Plan"].get("Node Type"),
        "indexes": indexes,
        "sequential_large_relations": sequential_large_relations,
        "shared_hit_blocks": sum(int(node.get("Shared Hit Blocks", 0)) for node in nodes),
        "shared_read_blocks": sum(int(node.get("Shared Read Blocks", 0)) for node in nodes),
        "plan": document["Plan"],
    }


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--database-url-file",
        type=pathlib.Path,
        help="file containing the libpq URI; otherwise use MTC_BENCH_DATABASE_URL",
    )
    parser.add_argument("--output", type=pathlib.Path)
    parser.add_argument("--max-execution-ms", type=float, default=250.0)
    parser.add_argument("--statement-timeout-ms", type=int, default=30000)
    parser.add_argument("--min-request-rows", type=int, default=100000)
    parser.add_argument("--allow-sequential-scan", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if shutil.which("psql") is None:
        print(
            json.dumps(
                {
                    "passed": False,
                    "exit_code": 3,
                    "error_kind": "prerequisite",
                    "error": "psql is not installed",
                }
            ),
            file=sys.stderr,
        )
        return 3
    try:
        database_url = (
            args.database_url_file.read_text().strip()
            if args.database_url_file is not None
            else os.environ.get("MTC_BENCH_DATABASE_URL", "").strip()
        )
    except OSError as error:
        print(
            json.dumps(
                {"passed": False, "exit_code": 3, "error_kind": "prerequisite", "error": str(error)}
            ),
            file=sys.stderr,
        )
        return 3
    if not database_url:
        print(
            json.dumps(
                {
                    "passed": False,
                    "exit_code": 3,
                    "error_kind": "prerequisite",
                    "error": "MTC_BENCH_DATABASE_URL or --database-url-file is required",
                }
            ),
            file=sys.stderr,
        )
        return 3
    repository = pathlib.Path(__file__).resolve().parents[2]
    output = (
        args.output or repository / "tests/load/results/postgres-explain-latest.json"
    ).resolve()
    output.parent.mkdir(parents=True, exist_ok=True)
    try:
        request_rows = int(scalar(database_url, "SELECT count(*) FROM request_records;", args.statement_timeout_ms))
        event_rows = int(scalar(database_url, "SELECT count(*) FROM request_events;", args.statement_timeout_ms))
        key_id = scalar(
            database_url,
            "SELECT key_id FROM request_records GROUP BY key_id ORDER BY count(*) DESC LIMIT 1;",
            args.statement_timeout_ms,
        )
        tenant_id = scalar(
            database_url,
            "SELECT tenant_id FROM request_records GROUP BY tenant_id ORDER BY count(*) DESC LIMIT 1;",
            args.statement_timeout_ms,
        )
        event_tenant_id = scalar(
            database_url,
            "SELECT tenant_id FROM request_events GROUP BY tenant_id ORDER BY count(*) DESC LIMIT 1;",
            args.statement_timeout_ms,
        )
        for name, value in (("key_id", key_id), ("tenant_id", tenant_id)):
            if not SAFE_ID.fullmatch(value):
                raise PrerequisiteFailure(f"sample {name} is absent or malformed")
        if event_tenant_id and not SAFE_ID.fullmatch(event_tenant_id):
            raise PrerequisiteFailure("sample event tenant id is malformed")
        error_code = literal(
            database_url,
            "SELECT error_code FROM request_records WHERE error_code IS NOT NULL GROUP BY error_code ORDER BY count(*) DESC LIMIT 1",
            args.statement_timeout_ms,
        )

        queries: list[tuple[str, str]] = [
            (
                "global_newest_cursor",
                "SELECT id, created_at, model, status_code FROM request_records "
                "ORDER BY created_at DESC, id DESC LIMIT 100",
            ),
            (
                "tenant_newest_cursor",
                "SELECT id, created_at, model, status_code FROM request_records "
                f"WHERE tenant_id = '{tenant_id}' ORDER BY created_at DESC, id DESC LIMIT 100",
            ),
            (
                "key_newest_cursor",
                "SELECT id, created_at, model, status_code FROM request_records "
                f"WHERE key_id = '{key_id}' ORDER BY created_at DESC, id DESC LIMIT 100",
            ),
            (
                "key_daily_aggregate",
                "SELECT day_bucket, SUM(requests), SUM(input_tokens), SUM(output_tokens), "
                "SUM(cost_micros) FROM usage_daily_aggregates "
                f"WHERE key_id = '{key_id}' GROUP BY day_bucket ORDER BY day_bucket",
            ),
        ]
        if error_code is not None:
            queries.append(
                (
                    "tenant_error_troubleshooting",
                    "SELECT id, created_at, model, status_code, error_code FROM request_records "
                    f"WHERE tenant_id = '{tenant_id}' AND error_code = {error_code} "
                    "ORDER BY created_at DESC, id DESC LIMIT 100",
                )
            )
        if event_tenant_id:
            queries.append(
                (
                    "tenant_event_cursor",
                    "SELECT event_id, event_at, request_id, event_kind FROM request_events "
                    f"WHERE tenant_id = '{event_tenant_id}' ORDER BY event_at ASC, event_id ASC LIMIT 500",
                )
            )

        results = [
            explain(database_url, name, query, args.statement_timeout_ms)
            for name, query in queries
        ]
        checks: list[dict[str, Any]] = [
            {
                "name": "large-volume request row precondition",
                "actual": request_rows,
                "operator": ">=",
                "expected": args.min_request_rows,
                "passed": request_rows >= args.min_request_rows,
            }
        ]
        for result in results:
            checks.append(
                {
                    "name": f"{result['name']} execution time",
                    "actual": result["execution_time_ms"],
                    "operator": "<=",
                    "expected": args.max_execution_ms,
                    "passed": result["execution_time_ms"] <= args.max_execution_ms,
                }
            )
            if not args.allow_sequential_scan and request_rows >= args.min_request_rows:
                checks.append(
                    {
                        "name": f"{result['name']} avoids sequential history scan",
                        "actual": result["sequential_large_relations"],
                        "operator": "==",
                        "expected": [],
                        "passed": not result["sequential_large_relations"],
                    }
                )
        report = {
            "schema_version": 1,
            "benchmark": "memeloop-token-center-postgres-explain",
            "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
            "postgres_version": scalar(database_url, "SHOW server_version;", args.statement_timeout_ms),
            "dataset": {"request_rows": request_rows, "event_rows": event_rows},
            "thresholds": {
                "max_execution_ms": args.max_execution_ms,
                "min_request_rows": args.min_request_rows,
                "allow_sequential_scan": args.allow_sequential_scan,
            },
            "results": results,
            "checks": checks,
            "passed": all(item["passed"] for item in checks),
        }
        report["exit_code"] = 0 if report["passed"] else 2
        output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n")
        print(json.dumps({"passed": report["passed"], "checks": checks}, indent=2))
        return int(report["exit_code"])
    except (PrerequisiteFailure, subprocess.SubprocessError, ValueError, json.JSONDecodeError) as error:
        print(
            json.dumps(
                {"passed": False, "exit_code": 3, "error_kind": "prerequisite", "error": str(error)}
            ),
            file=sys.stderr,
        )
        return 3


if __name__ == "__main__":
    raise SystemExit(main())
