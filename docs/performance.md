# Performance and memory acceptance

The repository contains two dependency-light, machine-readable acceptance tools for ARC-05, ARC-06 and MM-05. They are release gates, not synthetic throughput claims. Always record the commit, binary, machine/container limits, PostgreSQL version, dataset size and JSON result together.

## Memory, streaming and large assets

Build an optimized binary from the exact release commit, then run the 15-minute acceptance profile:

```bash
cargo build --release --bin memeloop-token-center
ops/benchmark-memory.sh --profile acceptance \
  --output tests/load/results/memory-$(git rev-parse --short HEAD).json
```

The harness needs Linux `/proc`, Python 3 and the Rust binary; it does not need k6, Docker, PostgreSQL, curl or jq. It creates a temporary SQLite database and filesystem archive, starts separate control, gateway and worker processes, provisions routes and a credential through HTTP, and removes all temporary data and processes on exit. No real upstream or secret is used.

The acceptance profile performs all of the following:

- measures idle RSS/PSS for the split control, gateway and worker roles;
- receives 12 concurrent 16 MiB upstream streams while sampling gateway RSS;
- seeds 100,000 tenants, upstreams, routes and managed service credentials,
  then requires 16 concurrent control-list reads to remain at 100 rows and
  below 1 MiB per response while sampling control RSS;
- runs two concurrent Responses-tool image generations whose decoded images
  are 11 MiB each, enforces the 16 MiB final JSON response cap, and samples
  gateway RSS;
- closes downstream connections early and requires the requests to become
  `downstream_disconnected` failures;
- sends 65 MiB from the upstream, proves the gateway delivers between 63 and
  64 MiB before enforcing its 64 MiB cap, and requires an
  `upstream_response_too_large` failure record;
- streams and archives a 500 MiB Seedance asset, requires a newly archived
  object of exactly 500 MiB, and samples gateway and worker separately;
- runs a 15-minute, rate-controlled soak, waits for cooldown, and gates retained RSS and RSS slope.

For quick local feedback, the short profile uses a 100 MiB asset and a 30-second soak. It covers the same paths, but its RSS slope is informational because such a short regression is statistically noisy:

```bash
ops/benchmark-memory.sh --profile short --binary target/debug/memeloop-token-center
```

Default release thresholds are deliberately well below the historical 1 GiB CPA process and leave explicit headroom under the chart's 256 MiB gateway limit:

| Measurement | Gate |
|---|---:|
| Gateway idle RSS | at most 96 MiB |
| Concurrent-stream gateway RSS increase | at most 128 MiB |
| 100k-row concurrent control-list RSS increase | at most 64 MiB |
| Two concurrent 11 MiB synchronous-image RSS increase | at most 128 MiB |
| 100–500 MiB asset gateway RSS increase | at most 96 MiB |
| 100–500 MiB asset worker RSS increase | at most 192 MiB |
| Gateway RSS retained after cooldown | at most 64 MiB over idle |
| 15-minute gateway RSS slope | at most 2 MiB/minute |
| Peak gateway RSS with a 256 MiB deployment limit | at most 224 MiB (32 MiB headroom) |
| Peak gateway RSS / user-observed 1 GiB CPA process | at most 25% |

Every threshold has a command-line override so a stricter deployment budget can be recorded explicitly. Do not raise a threshold merely to turn a regression green; attach a heap/profile investigation and explain the new budget.

The non-PR-blocking `memory-acceptance` GitHub Actions workflow accepts only an
exact 40-hex commit SHA (or defaults to the workflow event SHA). It validates
the resolved checkout, builds it in release mode with Rust 1.95.0 and embedded
build metadata, and runs the complete acceptance profile. Its failure-safe
artifact contains checkout, toolchain and build metadata, build/harness logs,
and the JSON report after a completed or caught-failure harness run. The artifact is retained for 30
days even when checkout, setup, build, functional or resource gates fail. The
report records the resolved Git commit and binary digest; use those fields—not
a mutable branch name—as release evidence. The harness rejects any report
labelled `acceptance` unless it includes a 500 MiB asset, a soak of at least 900
seconds, at least 12 concurrent streams, and at least 16 MiB per stream.

The workflow deliberately writes evidence under the runner's temporary
directory, outside the checkout. This keeps `git_dirty=false` meaningful and
prevents benchmark logs or reports from becoming accidental source changes.
Harness prerequisite and functional failures still write a machine-readable
failure report when the output location is writable.

The 224 MiB check measures the gateway process's Linux RSS/high-water mark and reserves 32 MiB for allocator, runtime and other container charges. It is a conservative release proxy, not a measurement of Kubernetes cgroup `memory.current` or `memory.peak`. Before production cutover, confirm the same workload in the 256 MiB Pod and retain cgroup peak plus `memory.events`; any OOM event fails the deployment gate regardless of this harness result.

The output has `schema_version`, raw measurements, thresholds as individual checks and one top-level `passed` value. Exit codes are stable:

- `0`: all functional and resource gates passed;
- `2`: the run completed but at least one threshold failed;
- `3`: a prerequisite or startup condition was missing;
- `4`: a functional test failed.

The asset test uses filesystem object storage so the worker's RSS result is not confused by the intentionally in-memory test archive. Production S3 should additionally be tested for multipart retry and latency, but the bounded-memory property is exercised here. The GitHub-hosted binary digest is not assumed to equal the GHCR image's binary digest: compare the report with the extracted release image binary, and retain the separate in-Pod cgroup acceptance evidence before cutover.

## PostgreSQL large-history query plans

Run the plan suite read-only against an imported, analyzed PostgreSQL snapshot with at least 100,000 request rows:

```bash
MTC_BENCH_DATABASE_URL='postgres://…' \
  ops/benchmark-postgres.sh \
  --max-execution-ms 250 \
  --min-request-rows 100000 \
  --output tests/load/results/postgres-explain-$(git rev-parse --short HEAD).json
```

Keep the URI in `MTC_BENCH_DATABASE_URL` or a mode-`0600` file passed with `--database-url-file`; it is intentionally not accepted as a command-line value, so credentials do not appear in process listings.

The URL is never written to the report. The script forces `default_transaction_read_only=on`, applies a statement timeout, selects high-cardinality tenant/key samples, and runs `EXPLAIN (ANALYZE, BUFFERS, FORMAT JSON, TIMING OFF)` for:

- global, tenant and credential newest-first cursor pages;
- operator-shaped tenant statistics using daily rollups plus exact UTC boundary
  facts, including model/protocol/status/error/upstream/route/alias/principal
  dimensions;
- credential daily aggregates;
- tenant error troubleshooting when an error sample exists;
- tenant request-event cursor replay when events exist.
- usage-analysis model, error and route drill-downs when samples exist.

It records execution/planning time, returned rows, buffer hits/reads, index names and the complete plan tree. On a sufficiently large dataset, sequential scans over more than 10,000 history rows fail the run; the sole exception is an explicitly time-bounded incomplete-day fact branch, which remains subject to the 250 ms latency gate. It also verifies request and generation fact coverage, non-empty historical currencies, required observability indexes, and request-history partition indexes. The CI migration-smoke job creates a disposable 100,000-row fixture with `tests/load/seed_postgres_observability.sql`, enforces the same plan gate, and retains its JSON report. Imported-snapshot and cold-cache evidence are still required when the deployment SLO depends on production cardinality or storage latency.

Schema v24 backfills terminal request facts and UTC daily rollups. If a legacy
load, repair, or retention operation bypassed normal dual writes, reconcile an
explicit range before benchmarking. The command is dry-run by default:

On PostgreSQL, the same migration installs the parent partitioned
`request_records_recent_idx` and `request_events_global_cursor_idx` indexes, so
fresh Helm deployments have global history and SSE cursor paths without a
manual operator step. Running
`ops/backfill-postgres-history-partitions.sh --indexes-only` remains the
low-lock repair path for databases that were created by an older build.

```bash
PGHOST=… PGUSER=… PGDATABASE=… \
  ops/reconcile-postgres-request-stats.sh \
  --from 2026-07-01 --before 2026-08-01 --max-days 31

PGHOST=… PGUSER=… PGDATABASE=… \
  ops/reconcile-postgres-request-stats.sh \
  --from 2026-07-01 --before 2026-08-01 --max-days 31 --apply
```

Statistics pruning is a separate, explicit operation. It refuses to delete request
or generation facts and rollups while either raw source still exists in the target interval and requires both
`--apply` and `--confirm-prune`; raw-history archival and deletion use their own
reviewed retention procedure.

Before treating results as comparable, run `ANALYZE`, use the same PostgreSQL settings and resource limits, and state whether the cache was warm. A report from a smaller dataset exits `2`: it can catch SQL breakage, but it is not ARC-05 large-volume evidence. The benchmark performs no inserts, schema changes or maintenance and is safe to point at a read replica.

## Evidence policy

Commit selected release reports under `tests/load/results/` only when they include the exact tested commit and an environment note in the release review. The `*-latest.json` names are convenient local outputs and should not replace dated/revisioned evidence. A short run proves harness functionality; ARC-06/MM-05 acceptance requires the optimized 15-minute/500 MiB profile, and ARC-05 requires the PostgreSQL report at imported-data scale.
