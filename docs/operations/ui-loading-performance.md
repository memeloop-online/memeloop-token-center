# Operator loading performance audit

This document describes request-dependency and loading-performance design. It
does not report production measurements or deployment acceptance.

| Surface | Observed dependency / cost | Priority |
| --- | --- | --- |
| Authentication / every page | Tenant discovery precedes page mount intentionally; returning to a page remounts its data workspace. There is no shared cross-page response cache. | Preserve authorization fencing; cancel stale reads before considering scoped deduplication. |
| Model pricing | Schema request gates mount, then price/usage/generation requests all wait for the slowest. Currency changes and price saves repeat currency-independent usage. Each model searches the entire prices array. | Fixed here: schema-independent mount, progressive price publication, separate tenant usage effect, abort/deadline, indexed memoized rows. |
| Upstream providers | Provider types, accounts, monitoring snapshot and account availability are parallel but one combined boundary waits for all four. Both monitoring representations are requested. | Decouple base account rendering from expensive availability; retain exact-account semantics. |
| Model routes | Provider types/accounts gate RouteWorkspace; group and credential option reads occur inside the mounted workspace. | Render route list independently of editor metadata; debounce/cancel searchable options. |
| Requests | Request and filter dependencies are separate modules; stream runs only for selected Requests/Sessions views. | Separate owner reviewing query fan-out; do not infer a single endpoint bottleneck without timing. |
| Sessions | List, selected detail, pagination and replay archive are distinct requests with cancellation. | Measure list versus archive separately; never eager-fetch all replay bodies. |
| Usage | Upstream metadata and usage query already render independently. | Check query changes for redundant fetches and measure aggregation server time. |
| Overview | Request list and monitoring snapshot use separate resources. | Separate owner reviewing rendering and monitoring costs. |
| Generation tasks | Initial job list, then selected job detail; action-specific reads. | Preserve on-demand detail, bound/cancel superseded list reads. |
| Client/service credentials | Schema-gated editor/list mounts; additional limits/routing are separate reads. | Separate credential owner; avoid globally fetching per-key metadata. |
| Plugins | Shell fetches plugin manifests; plugin page reads manifests again, then one configuration request per configurable plugin. | Explicit N-request fan-out; lazy configuration reads or a bounded batch contract. |
| Tenants | Tenant discovery already occurs at authentication; management owns another list lifecycle. | Measure repeat navigation; scoped invalidation required before caching. |
| Settings | Routes and filter-assistant settings load together for selected tenant. | Editor option dependency, not evidence of backend query latency. |

## Pricing backend follow-up

`model_price_usage_summary` invokes the existing filtered operator statistics
query and discards every projection except `by_model`. PostgreSQL uses one
materialized source and GROUPING SETS for summary/model/day/error; SQLite
materializes once then computes four projections. This is **not** a per-model
SQL N+1, but pricing pays for three unused projections. A future specialized
model-only query should preserve tenant, time/filter, terminal/generation,
currency aggregation and top-100 semantics, with query-plan and parity tests.
Do not replace it with an unbounded raw request scan or silently shorten the
window. Price list endpoints are already paginated; page completeness is a
separate product/API concern, not solved by raising limits.

## Verification targets

Under a deliberately delayed schema/usage response, saved token prices must
render as soon as their own request resolves. Changing USD/CNY must not issue a
new usage-summary request. Scope/unmount must cancel pending reads; stale
currency/scope responses must not overwrite current tables. Waiting reads have
a ten-second deadline and surface a partial-data error. Manual edits and sync
retain their mutation endpoints and authorization rules.

The added source contracts protect these dependency/cancellation invariants.
CI/browser acceptance and a sequential read-only production timing capture
remain necessary before claiming a measured latency improvement.
