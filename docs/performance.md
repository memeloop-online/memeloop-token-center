# Performance and memory acceptance

The repository contains two dependency-light, machine-readable acceptance tools for ARC-05, ARC-06 and MM-05. They are release gates, not synthetic throughput claims. Always record the commit, binary, machine/container limits, PostgreSQL version, dataset size and JSON result together.

## Memory, streaming and large assets

Build the same optimized binary that will be deployed, then run the 15-minute acceptance profile:

```bash
cargo build --release --bin memeloop-token-center
ops/benchmark-memory.sh --profile acceptance \
  --output tests/load/results/memory-$(git rev-parse --short HEAD).json
```

The harness needs Linux `/proc`, Python 3 and the Rust binary; it does not need k6, Docker, PostgreSQL, curl or jq. It creates a temporary SQLite database and filesystem archive, starts separate control, gateway and worker processes, provisions routes and a credential through HTTP, and removes all temporary data and processes on exit. No real upstream or secret is used.

The acceptance profile performs all of the following:

- measures idle RSS/PSS for the split control, gateway and worker roles;
- receives 12 concurrent 16 MiB upstream streams while sampling gateway RSS;
- closes downstream connections early and requires the requests to become `upstream_stream` failures;
- sends 65 MiB from the upstream and proves the gateway stops at its 64 MiB cap;
- streams and archives a 500 MiB Seedance asset while sampling gateway and worker separately;
- runs a 15-minute, rate-controlled soak, waits for cooldown, and gates retained RSS and RSS slope.

For quick local feedback, the short profile uses a 100 MiB asset and a 30-second soak. It covers the same paths, but its RSS slope is informational because such a short regression is statistically noisy:

```bash
ops/benchmark-memory.sh --profile short --binary target/debug/memeloop-token-center
```

Default release thresholds are deliberately well below the historical 1 GiB CPA process:

| Measurement | Gate |
|---|---:|
| Gateway idle RSS | at most 256 MiB |
| Concurrent-stream gateway RSS increase | at most 192 MiB |
| 100–500 MiB asset gateway RSS increase | at most 96 MiB |
| 100–500 MiB asset worker RSS increase | at most 192 MiB |
| Gateway RSS retained after cooldown | at most 96 MiB over idle |
| 15-minute gateway RSS slope | at most 2 MiB/minute |
| Peak gateway RSS / user-observed 1 GiB CPA process | at most 25% |

Every threshold has a command-line override so a stricter deployment budget can be recorded explicitly. Do not raise a threshold merely to turn a regression green; attach a heap/profile investigation and explain the new budget.

The output has `schema_version`, raw measurements, thresholds as individual checks and one top-level `passed` value. Exit codes are stable:

- `0`: all functional and resource gates passed;
- `2`: the run completed but at least one threshold failed;
- `3`: a prerequisite or startup condition was missing;
- `4`: a functional test failed.

The asset test uses filesystem object storage so the worker's RSS result is not confused by the intentionally in-memory test archive. Production S3 should additionally be tested for multipart retry and latency, but the bounded-memory property is exercised here.

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
- credential daily aggregates;
- tenant error troubleshooting when an error sample exists;
- tenant request-event cursor replay when events exist.

It records execution/planning time, returned rows, buffer hits/reads, index names and the complete plan tree. On a sufficiently large dataset, sequential scans of `request_records` or `request_events` fail the run. The default latency budget is 250 ms per query, but release evidence should also include cold-cache results if the deployment SLO depends on them.

Before treating results as comparable, run `ANALYZE`, use the same PostgreSQL settings and resource limits, and state whether the cache was warm. A report from a smaller dataset exits `2`: it can catch SQL breakage, but it is not ARC-05 large-volume evidence. The benchmark performs no inserts, schema changes or maintenance and is safe to point at a read replica.

## Evidence policy

Commit selected release reports under `tests/load/results/` only when they include the exact tested commit and an environment note in the release review. The `*-latest.json` names are convenient local outputs and should not replace dated/revisioned evidence. A short run proves harness functionality; ARC-06/MM-05 acceptance requires the optimized 15-minute/500 MiB profile, and ARC-05 requires the PostgreSQL report at imported-data scale.
