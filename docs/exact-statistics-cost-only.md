# Exact statistics cost-only correction

`backfill-exact-failed-request-costs --manifest reviewed.json --cost-only`
previews the existing strict USD, contract-ceiling failure manifest. `--apply`
is still required to write. Without `--cost-only`, the original full-rebuild
path and exclusive statistics sentinel remain unchanged.

The cost-only write set is explicitly limited to:

- Immutable v2 correction markers for the exact manifest.
- Compare-and-set `request_stats_facts.cost_micros` for those requests.
- `cost_micros` in `usage_daily_aggregates`, `request_daily_aggregates`,
  `session_usage_totals`, `session_usage_hourly`, `session_usage_daily`,
  `usage_analysis_hourly` and `usage_analysis_daily`.

Raw requests, reservations, ledger entries, settlement feeds, balances, account
usage, key budgets, tokens, request counts and every other statistics field are
not rewritten. The shared statistics sentinel fences full rebuilds and pruning
while remaining compatible with deployed append workers. Grouped, checked cost
deltas use atomic conditional UPDATEs and PostgreSQL row locks; they never
replace a concurrently updated row from an earlier snapshot. Fact row locks,
compare-and-set and immutable markers make concurrent corrections and replay
exactly once.

A missing aggregate or insufficient cost rejects and rolls back the whole
transaction, including markers and fact changes. This mode is not a general
repair of pre-existing inconsistent counters. A successful replay changes zero
rows. Reports identify `projection_mode: cost-only` and report zero full-rebuild
counts. The original 5-second statement, 1-second lock and one-connection CLI
bounds remain unchanged. An unknown commit is observed, not automatically
retried. Source review and CI do not authorize a production execution window.

The existing SQLite and PostgreSQL backfill contracts cover exact-mode preview,
replay, rejected evidence, transaction rollback, all-column financial and
non-cost statistics snapshots, shared writer coexistence, and actual append
writer interleaving. PostgreSQL runs use the existing CI database service and
`MTC_TEST_POSTGRES_URL`; no local test run or new test framework is required.

The dedicated PostgreSQL concurrent-contribution contract runs both writer-first
and correction-first orders for a real request terminal writer and a real
generation terminal worker. Database blockers prove overlap on the same physical
aggregate row. For requests, all seven aggregate dimensions are identical and
retain both request counts; generation shares the session total and retains its
cost, count and units. Each order checks every financial column and every
non-cost statistics column across the correction commit, plus zero-change replay.
Aggregate failures identify only the fixed stage and table category, never
request IDs, model names, tenant IDs, dimension values or payloads.
