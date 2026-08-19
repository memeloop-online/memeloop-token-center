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
LARGE_HISTORY_RELATIONS = (
    "request_records",
    "request_events",
    "request_stats_facts",
    "request_daily_aggregates",
    "generation_jobs",
    "generation_stats_facts",
    "generation_daily_aggregates",
    "usage_analysis_hourly",
    "usage_analysis_daily",
)


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


def explain(
    database_url: str,
    name: str,
    query: str,
    timeout_ms: int,
    max_sequential_scan_rows: int,
) -> dict[str, Any]:
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
    sequential_scans = []
    for node in nodes:
        relation = str(node.get("Relation Name", ""))
        if node.get("Node Type") != "Seq Scan" or not relation.startswith(
            LARGE_HISTORY_RELATIONS
        ):
            continue
        loops = max(1, int(node.get("Actual Loops", 1)))
        scanned_rows = (
            int(node.get("Actual Rows", 0))
            + int(node.get("Rows Removed by Filter", 0))
        ) * loops
        sequential_scans.append({"relation": relation, "scanned_rows": scanned_rows})
    sequential_large_relations = sorted(
        scan["relation"]
        for scan in sequential_scans
        if scan["scanned_rows"] > max_sequential_scan_rows
    )
    return {
        "name": name,
        "planning_time_ms": document.get("Planning Time", 0.0),
        "execution_time_ms": document.get("Execution Time", 0.0),
        "returned_rows": document["Plan"].get("Actual Rows", 0),
        "root_node": document["Plan"].get("Node Type"),
        "indexes": indexes,
        "relations": sorted(
            {
                str(node["Relation Name"])
                for node in nodes
                if node.get("Relation Name") is not None
            }
        ),
        "sequential_large_relations": sequential_large_relations,
        "sequential_scans": sequential_scans,
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
    parser.add_argument("--max-sequential-scan-rows", type=int, default=10000)
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
        request_rows = int(
            scalar(
                database_url,
                "SELECT count(*) FROM request_records;",
                args.statement_timeout_ms,
            )
        )
        event_rows = int(
            scalar(
                database_url,
                "SELECT count(*) FROM request_events;",
                args.statement_timeout_ms,
            )
        )
        fact_rows = int(
            scalar(
                database_url,
                "SELECT count(*) FROM request_stats_facts;",
                args.statement_timeout_ms,
            )
        )
        generation_fact_rows = int(
            scalar(
                database_url,
                "SELECT count(*) FROM generation_stats_facts;",
                args.statement_timeout_ms,
            )
        )
        terminal_request_rows = int(
            scalar(
                database_url,
                "SELECT count(*) FROM request_records "
                "WHERE completed_at IS NOT NULL AND status_code IS NOT NULL;",
                args.statement_timeout_ms,
            )
        )
        terminal_generation_rows = int(
            scalar(
                database_url,
                "SELECT count(*) FROM generation_jobs "
                "WHERE status IN ('succeeded', 'failed', 'cancelled');",
                args.statement_timeout_ms,
            )
        )
        blank_currency_rows = int(
            scalar(
                database_url,
                "SELECT SUM(rows) FROM ("
                "SELECT count(*) AS rows FROM request_records WHERE currency = '' UNION ALL "
                "SELECT count(*) FROM request_stats_facts WHERE currency = '' UNION ALL "
                "SELECT count(*) FROM request_daily_aggregates WHERE currency = '' UNION ALL "
                "SELECT count(*) FROM generation_stats_facts WHERE currency = '' UNION ALL "
                "SELECT count(*) FROM generation_daily_aggregates WHERE currency = '' UNION ALL "
                "SELECT count(*) FROM usage_analysis_hourly WHERE currency = '' UNION ALL "
                "SELECT count(*) FROM usage_analysis_daily WHERE currency = '') currency_gaps;",
                args.statement_timeout_ms,
            )
        )
        valid_required_cursor_indexes = int(
            scalar(
                database_url,
                "SELECT count(*) FROM pg_index WHERE indexrelid IN "
                "(to_regclass('public.request_records_recent_idx'), "
                "to_regclass('public.request_events_global_cursor_idx')) "
                "AND indisvalid AND indisready;",
                args.statement_timeout_ms,
            )
        )
        unattached_required_cursor_leaves = int(
            scalar(
                database_url,
                "WITH required(parent_table, parent_index) AS (VALUES "
                "('request_records', 'request_records_recent_idx'), "
                "('request_events', 'request_events_global_cursor_idx')) "
                "SELECT count(*) FROM required r "
                "JOIN pg_inherits table_inheritance "
                "ON table_inheritance.inhparent = to_regclass('public.' || r.parent_table) "
                "WHERE NOT EXISTS (SELECT 1 FROM pg_inherits index_inheritance "
                "JOIN pg_index child_index ON child_index.indexrelid = index_inheritance.inhrelid "
                "WHERE index_inheritance.inhparent = to_regclass('public.' || r.parent_index) "
                "AND child_index.indrelid = table_inheritance.inhrelid "
                "AND child_index.indisvalid AND child_index.indisready);",
                args.statement_timeout_ms,
            )
        )
        valid_required_observability_indexes = int(
            scalar(
                database_url,
                "SELECT count(*) FROM pg_index WHERE indexrelid IN ("
                "to_regclass('public.request_stats_facts_tenant_created_idx'), "
                "to_regclass('public.generation_stats_facts_tenant_created_idx'), "
                "to_regclass('public.request_daily_aggregates_tenant_day_idx'), "
                "to_regclass('public.generation_daily_aggregates_tenant_day_idx'), "
                "to_regclass('public.usage_analysis_hourly_tenant_time_idx'), "
                "to_regclass('public.usage_analysis_daily_tenant_time_idx'), "
                "to_regclass('public.usage_analysis_daily_tenant_model_time_idx'), "
                "to_regclass('public.usage_analysis_daily_tenant_error_time_idx'), "
                "to_regclass('public.usage_analysis_daily_tenant_route_time_idx')) "
                "AND indisvalid AND indisready;",
                args.statement_timeout_ms,
            )
        )
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
        analysis_model = literal(
            database_url,
            "SELECT model FROM usage_analysis_daily "
            f"WHERE tenant_id = '{tenant_id}' GROUP BY model ORDER BY count(*) DESC LIMIT 1",
            args.statement_timeout_ms,
        )
        analysis_error = literal(
            database_url,
            "SELECT error_code FROM usage_analysis_daily "
            f"WHERE tenant_id = '{tenant_id}' AND error_code <> '' "
            "GROUP BY error_code ORDER BY count(*) DESC LIMIT 1",
            args.statement_timeout_ms,
        )
        analysis_route = literal(
            database_url,
            "SELECT model_route_id FROM usage_analysis_daily "
            f"WHERE tenant_id = '{tenant_id}' AND model_route_id <> '' "
            "GROUP BY model_route_id ORDER BY count(*) DESC LIMIT 1",
            args.statement_timeout_ms,
        )
        stats_day_bounds = scalar(
            database_url,
            "SELECT min(day_bucket)::text || '|' || max(day_bucket)::text "
            f"FROM request_daily_aggregates WHERE tenant_id = '{tenant_id}';",
            args.statement_timeout_ms,
        )
        try:
            stats_min_day, stats_max_day = (int(value) for value in stats_day_bounds.split("|", 1))
        except (TypeError, ValueError) as error:
            raise PrerequisiteFailure("request daily aggregates are absent for the sample tenant") from error
        stats_from_day = max(stats_min_day, stats_max_day - 92)
        stats_from = stats_from_day * 86_400_000 + 1
        stats_to = (stats_max_day + 1) * 86_400_000 - 2
        stats_full_day_from = (stats_from + 86_400_000 - 1) // 86_400_000 * 86_400_000
        stats_full_day_to = (stats_to + 1) // 86_400_000 * 86_400_000
        filtered_stats_query = (
            "WITH filtered_activity AS MATERIALIZED ("
            "SELECT day_bucket * 86400000 AS created_at, model, status_class, error_code, "
            "requests, input_tokens, output_tokens, cost_micros "
            "FROM request_daily_aggregates "
            f"WHERE tenant_id = '{tenant_id}' "
            f"AND day_bucket >= {stats_full_day_from} / 86400000 "
            f"AND day_bucket < {stats_full_day_to} / 86400000 "
            "UNION ALL "
            "SELECT created_at, model, status_class, error_code, 1, input_tokens, "
            "output_tokens, cost_micros FROM request_stats_facts "
            f"WHERE tenant_id = '{tenant_id}' AND created_at >= {stats_from} "
            f"AND created_at <= {stats_to} "
            f"AND (created_at < {stats_full_day_from} OR created_at >= {stats_full_day_to})"
            "), enriched AS (SELECT model, created_at / 86400000 AS day_bucket, "
            "NULLIF(error_code, '') AS error_bucket, status_class, requests, input_tokens, "
            "output_tokens, cost_micros FROM filtered_activity) "
            "SELECT model, day_bucket, error_bucket, SUM(requests), SUM(input_tokens), "
            "SUM(output_tokens), SUM(cost_micros) FROM enriched "
            "GROUP BY GROUPING SETS ((), (model), (day_bucket), (error_bucket)) "
            "HAVING GROUPING(error_bucket) = 1 OR error_bucket IS NOT NULL"
        )
        usage_day_bounds = scalar(
            database_url,
            "SELECT min(day_bucket)::text || '|' || max(day_bucket)::text "
            f"FROM usage_analysis_daily WHERE tenant_id = '{tenant_id}';",
            args.statement_timeout_ms,
        )
        try:
            usage_min_day, usage_max_day = (
                int(value) for value in usage_day_bounds.split("|", 1)
            )
        except (TypeError, ValueError) as error:
            raise PrerequisiteFailure(
                "usage analysis daily rollups are absent for the sample tenant"
            ) from error
        usage_from_day = max(usage_min_day, usage_max_day - 92)
        usage_daily_query = (
            "WITH filtered_activity AS MATERIALIZED ("
            "SELECT * FROM usage_analysis_daily "
            f"WHERE tenant_id = '{tenant_id}' AND day_bucket >= {usage_from_day} "
            f"AND day_bucket <= {usage_max_day}) "
            "SELECT currency, SUM(requests), "
            "SUM(input_tokens), SUM(cached_input_tokens), SUM(cache_write_tokens), "
            "SUM(generation_units), SUM(duration_sum_ms), SUM(cost_micros) "
            "FROM filtered_activity GROUP BY GROUPING SETS "
            "((currency), (day_bucket, currency), (model, currency), "
            "(key_id, currency), (upstream_account_id, currency), "
            "(protocol, currency), (status_class, currency), (error_code, currency))"
        )
        usage_hour_bounds = scalar(
            database_url,
            "SELECT min(hour_bucket)::text || '|' || max(hour_bucket)::text "
            f"FROM usage_analysis_hourly WHERE tenant_id = '{tenant_id}';",
            args.statement_timeout_ms,
        )
        try:
            usage_min_hour, usage_max_hour = (
                int(value) for value in usage_hour_bounds.split("|", 1)
            )
        except (TypeError, ValueError) as error:
            raise PrerequisiteFailure(
                "usage analysis hourly rollups are absent for the sample tenant"
            ) from error
        usage_from_hour = max(usage_min_hour, usage_max_hour - 31 * 24)
        usage_hourly_query = (
            "SELECT hour_bucket, currency, protocol, SUM(requests), SUM(input_tokens), "
            "SUM(cached_input_tokens), SUM(cache_write_tokens), SUM(cost_micros) "
            "FROM usage_analysis_hourly "
            f"WHERE tenant_id = '{tenant_id}' AND hour_bucket >= {usage_from_hour} "
            f"AND hour_bucket <= {usage_max_hour} "
            "GROUP BY hour_bucket, currency, protocol ORDER BY hour_bucket, currency, protocol"
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
            ("tenant_filtered_stats", filtered_stats_query),
            ("tenant_usage_analysis_daily_93d", usage_daily_query),
            ("tenant_usage_analysis_hourly_31d", usage_hourly_query),
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
        for name, column, value in (
            ("tenant_usage_model_drilldown", "model", analysis_model),
            ("tenant_usage_error_drilldown", "error_code", analysis_error),
            ("tenant_usage_route_drilldown", "model_route_id", analysis_route),
        ):
            if value is not None:
                queries.append(
                    (
                        name,
                        "SELECT currency, SUM(requests), SUM(cost_micros) "
                        "FROM usage_analysis_daily "
                        f"WHERE tenant_id = '{tenant_id}' AND {column} = {value} "
                        f"AND day_bucket >= {usage_from_day} AND day_bucket <= {usage_max_day} "
                        "GROUP BY currency",
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
            explain(
                database_url,
                name,
                query,
                args.statement_timeout_ms,
                args.max_sequential_scan_rows,
            )
            for name, query in queries
        ]
        checks: list[dict[str, Any]] = [
            {
                "name": "large-volume request row precondition",
                "actual": request_rows,
                "operator": ">=",
                "expected": args.min_request_rows,
                "passed": request_rows >= args.min_request_rows,
            },
            {
                "name": "terminal request fact coverage",
                "actual": fact_rows,
                "operator": "==",
                "expected": terminal_request_rows,
                "passed": fact_rows == terminal_request_rows,
            },
            {
                "name": "terminal generation fact coverage",
                "actual": generation_fact_rows,
                "operator": "==",
                "expected": terminal_generation_rows,
                "passed": generation_fact_rows == terminal_generation_rows,
            },
            {
                "name": "historical billing currency is complete",
                "actual": blank_currency_rows,
                "operator": "==",
                "expected": 0,
                "passed": blank_currency_rows == 0,
            },
            {
                "name": "required global cursor parent indexes are ready",
                "actual": valid_required_cursor_indexes,
                "operator": "==",
                "expected": 2,
                "passed": valid_required_cursor_indexes == 2,
            },
            {
                "name": "required global cursor indexes cover every partition",
                "actual": unattached_required_cursor_leaves,
                "operator": "==",
                "expected": 0,
                "passed": unattached_required_cursor_leaves == 0,
            },
            {
                "name": "required observability indexes are ready",
                "actual": valid_required_observability_indexes,
                "operator": "==",
                "expected": 9,
                "passed": valid_required_observability_indexes == 9,
            },
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
                bounded_edge_fact_scan = (
                    result["name"] == "tenant_filtered_stats"
                    and set(result["sequential_large_relations"])
                    <= {"request_stats_facts", "generation_stats_facts"}
                )
                checks.append(
                    {
                        "name": f"{result['name']} avoids sequential history scan",
                        "actual": result["sequential_large_relations"],
                        "operator": "==",
                        "expected": "[] except bounded incomplete-day fact branches",
                        "passed": not result["sequential_large_relations"]
                        or bounded_edge_fact_scan,
                    }
                )
        report = {
            "schema_version": 3,
            "benchmark": "memeloop-token-center-postgres-explain",
            "generated_at": dt.datetime.now(dt.timezone.utc).isoformat(),
            "postgres_version": scalar(database_url, "SHOW server_version;", args.statement_timeout_ms),
            "dataset": {
                "request_rows": request_rows,
                "terminal_request_rows": terminal_request_rows,
                "request_fact_rows": fact_rows,
                "terminal_generation_rows": terminal_generation_rows,
                "generation_fact_rows": generation_fact_rows,
                "blank_currency_rows": blank_currency_rows,
                "event_rows": event_rows,
            },
            "thresholds": {
                "max_execution_ms": args.max_execution_ms,
                "min_request_rows": args.min_request_rows,
                "max_sequential_scan_rows": args.max_sequential_scan_rows,
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
