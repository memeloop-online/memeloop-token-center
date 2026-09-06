# Performance contract

## Objectives

Gateway capacity is bounded before request bodies or upstream responses can create
unbounded memory pressure. The service must remain responsive under concurrent
credentials, streaming requests, archive operations and provider failures.

## Controls

- Gateway body-read permits bound concurrent buffered bodies.
- Responses has an independent, smaller body-read permit pool and size limit.
- Streaming response delivery, request events, archive writes, generation workers
  and plugin calls each have explicit concurrency, byte or time bounds.
- Database queries use stable keyset cursors and bounded time windows. Large-history
  screens use precomputed summaries or ordered top-N queries rather than raw scans.
- Routing health probes and retry budgets are bounded; unhealthy accounts yield to
  another eligible account.
- Object-store failure is visible but does not remove unrelated text requests.

## Measurement

Release acceptance measures RSS, allocator state, active requests, stream count,
queue depth, database pool waits, latency percentiles, error rate and archive
latency on production-shaped data. Thresholds are expressed against the current
chart resource limits and observed baseline, not a retired process comparison.

Inspect query plans for request, statistic and conversation filters at representative
cardinality. Maintain covering indexes with forward migrations and measure build
lock duration on an isolated restored snapshot before a release. A performance
result is valid only for the stated data size, image digest and configuration.

## Failure behavior

Over-limit bodies fail with 413 before JSON parsing. Saturated bounded capacity
fails with 503. Credential rate or concurrency policy may return 429. Failures
must be classified consistently in API responses, structured logs and low-cardinality
metrics without including secret or request-content data.

## Evidence

GitHub Actions runs the deterministic performance and memory gates. Production
rollout records include the exact digest, resource limits, traffic shape, measured
latency, RSS, error rate and recovery observations. Do not raise a threshold merely
to make a failing run pass; identify the bounded resource or query plan responsible.
