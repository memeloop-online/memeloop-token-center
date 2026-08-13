# Product requirement acceptance matrix

Snapshot: 2026-08-14. The current working tree, not earlier status messages, is authoritative for this matrix.

Status meanings:

- **Implemented**: source and a directly relevant repository test/contract exist. Exact local and isolated-PostgreSQL verification completed for this snapshot is listed below.
- **Pending verification**: implementation exists in the current working tree, but end-to-end, deployed, migration, browser, load, or security evidence required by the requirement is incomplete.
- **Pending wiring**: lower-level pieces exist, but the user-visible workflow or HTTP contract is incomplete.
- **Missing**: no adequate implementation was found.

## Architecture, storage, and deployment

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| ARC-01 | New independent Rust service on `master` | Implemented | `Cargo.toml`, `src/main.rs`; repository branch is `master`; `/version` exposes injected build identity | Keep release metadata and compatibility text synchronized with releases. |
| ARC-02 | PostgreSQL is the production default | Implemented | `Config::from_env`, default `MTC_DATABASE_URL`; `DatabaseBackend::PostgreSql` | Deployed connection/failover test belongs to operations acceptance. |
| ARC-03 | SQLite for tests | Implemented | `DatabaseBackend::Sqlite`, working-tree migrations through v23; default Cucumber and targeted entitlement/route/metrics/observability/session-archive suites use SQLite; SQLite v21 locator plus v22 budget and v23 generation tests passed, and the corresponding fresh restricted PostgreSQL v1-v23 gate passed | Continue parity checks for every migration. |
| ARC-04 | S3 archive in production, simple memory/filesystem test backends | Implemented | `ArchiveStore::from_config` and bounded-read unit tests | Real S3 outage/retry and retention verification remains. |
| ARC-05 | Optimized large-volume request/event/aggregate schema | Pending verification | PostgreSQL partitions and query indexes under `migrations/postgres`; aggregate queries in `src/db.rs` | Provide EXPLAIN/latency evidence at imported-data scale and retention/partition lifecycle evidence. |
| ARC-06 | Low-memory gateway compared with CPA | Short release acceptance verified | bounded bodies, streaming archive, small SQL pool and image semaphore; 500 MiB asset plus 15-minute release acceptance passed | Record RSS/throughput results in the release artifact and run a longer production-shape leak soak before claiming long-term stability. |
| ARC-07 | Gateway/control/worker separation | Implemented | `RuntimeRole`, `router_for_role`, Helm role deployments | Verify public ingress never routes `/internal/v1/*`. |
| ARC-08 | Helm/K8s deployment | Implemented | `charts/memeloop-token-center` | Chart smoke test and upgrade/rollback acceptance remain. |
| ARC-09 | Production readiness/liveness separation | Implemented | anonymous `/livez`; bounded/coalesced DB+archive `/readyz`; deprecated `/healthz`; Helm probes; metrics integration tests 3/3 | Verify real dependency outage behavior in a deployed K8s environment. |
| ARC-10 | Prometheus metrics and ServiceMonitor | Implemented | control-only `metrics:read` `/metrics`, bounded labels, ServiceMonitor, metrics integration 3/3 and unit 3/3; Helm/kubeconform checks passed | Verify Prometheus scrape, alert rules, and dashboards in the deployed cluster. |
| ARC-11 | Partition maintenance does not wedge on DEFAULT rows | PostgreSQL verified | per-partition savepoints classify only PostgreSQL `23514` overlap as blocked, preserve rows in DEFAULT, report/warn and continue other tables/days; the fresh restricted PostgreSQL DB regression gate passed 3/3 including this path | This is fail-soft, not automatic evacuation: operations must safely drain each blocked date before retry creates its range partition. |
| ARC-12 | Globally unique request/event identity with partition pruning | Functional PostgreSQL verified | schema v21 non-partitioned locator tables own global IDs and carry partition timestamps; write/detail/finish/archive-import paths use exact `(id, created_at)` leaf access; SQLite lifecycle tests and real-PostgreSQL duplicate-backfill fail-closed plus single-leaf pruning regressions passed | The 141k-row migration lock-time and imported-scale EXPLAIN/latency evidence remain. |
| ARC-13 | Budget/rate hot paths do not scan unbounded audit history | Functional PostgreSQL verified | schema v22 transactionally maintains lifetime/reserved and account cumulative state, UTC-day plus rolling-week boundary detail; reserve/settle/cancel/grant-reversal no longer scan audit ledger/reservations; SQLite functional gates and the real-PostgreSQL concurrent reservation/settlement replay test passed | Production-history benchmark remains. Pre-v22 writers are not compatible after migration, so rollout requires a write barrier or a future dual-write bridge. |
| ARC-14 | Generation statistics avoid full job-table aggregation | Functional PostgreSQL verified | schema v23 terminal facts plus daily aggregates; ordinary bounded statistics use full-day aggregates and exact boundary facts, while pending/duration/cost filters use a bounded indexed raw fallback; SQLite backfill/idempotence/SQL-shape gates and the real-PostgreSQL generation aggregate integration passed | Imported-scale EXPLAIN/latency evidence remains. |

## Credentials, tenant isolation, and memeloop web

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| CRED-01 | Stable UUID identity and retry-safe rotation | Implemented | Required idempotency header and encrypted 24h replay; Cucumber replay; isolated K3S dev PostgreSQL proved 8 concurrent same-key calls produce generation 2 and four distinct keys produce generations 3–6 | Production rollout still needs normal monitoring; the isolated test schema was cleaned up. |
| CRED-02 | Migrated CPA credential can remain unchanged | Implemented | legacy credential table/API and CPA compatibility Cucumber scenario | Verify the production migrated Linux Codex identity after every importer run. |
| CRED-03 | Create credential with alias, model permission, quota and limits | Implemented | `POST /internal/v1/keys`, `KeyPolicy`, JSON Schema | Browser and deployed API regression still required for releases. |
| CRED-04 | Empty allowed-model list is fail-closed | Pending verification | `KeyPolicy::allows_model` requires `*` or exact match; OpenAPI documents this | Add explicit API/Cucumber assertion for `[]` denying all models. |
| CRED-05 | List/reconcile by tenant and principal | Pending verification | `GET /internal/v1/keys`; `Database::list_managed_keys` | Add pagination and verify tenant-scoped/global behavior against PostgreSQL. |
| CRED-06 | Suspend/reactivate/revoke client credential | Pending verification | `PATCH .../keys/{key_id}/status`; terminal revoke logic | Add end-to-end tests for every transition and request rejection. |
| CRED-07 | Service credential creation and retry-safe rotation | Implemented | service principal generations and required idempotency header; Cucumber exact-response replay; isolated K3S dev PostgreSQL proved 8 concurrent same-key calls produce generation 2 | Production rollout still needs normal monitoring; the isolated test schema was cleaned up. |
| CRED-08 | List/suspend/revoke service credentials | Pending verification | list/status endpoints and DB methods in current working tree | Add global-only authorization and transition E2E tests. |
| CRED-09 | Tenant-scoped service credential cannot cross tenant | Implemented | `require_service_tenant`, `management_tenant`; default Cucumber authorization-matrix scenario passed global, scoped, and downstream boundaries | Extend the matrix when new control endpoints are added. |
| CRED-10 | Least-privilege read/write scopes | Pending verification | read/write scopes include credentials, credits, entitlements, routes, prices and service tokens; operational metadata uses `metrics:read`; targeted entitlement/route/metrics denial tests pass | Complete the table-driven scope matrix for every OpenAPI operation. |
| CRED-11 | Registration create is retry-safe | Implemented | tenant-qualified idempotency key, canonical request hash, encrypted 24h response | Document caller persistence/recovery; covered in `api-contract.md`. |
| CRED-12 | Lost registration response can be reconciled | Pending verification | new managed-key list returns `account_id`, key ID, fingerprint and balance | Secret recovery intentionally requires rotation; add ambiguous-timeout integration test. |
| CRED-13 | User may use own credential only for own statistics/history | Implemented | `/self/v1/*` binds `key_id`; Cucumber authorization matrix and Playwright portal dogfood passed with non-empty own requests/statistics | Continue IDOR tests for generation and conversation detail. |
| CRED-14 | User credential has no administrative authority | Implemented | separate service/client authentication and role routers; Cucumber denial step | None for core boundary. |

## Subscription credit and billing

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| BILL-01 | Initial balance at credential provisioning | Implemented | atomic account/key/initial ledger creation in `Database::create_key` | None for basic flow. |
| BILL-02 | Idempotent subscription credit grant | Pending verification | `POST .../grants`, account-qualified idempotency index | Prove replay with same amount/source and rejection with different amount/source. |
| BILL-03 | Inspect balance and recent ledger from memeloop web | Pending verification | managed-key response includes available/reserved; ledger endpoint exists | Add pagination and PostgreSQL tenant-scope E2E tests. |
| BILL-04 | Cancel/replace subscription after partial consumption | Implemented | versioned entitlement cancel/replace withdraw only FIFO-derived remaining credit and retain consumed audit; SQLite lifecycle/usage tests passed | PostgreSQL concurrency/upgrade execution and memeloop web integration remain. |
| BILL-05 | Exact entitlement reconciliation | Implemented | GET/PUT/cancel/replace API, stable subscription/cycle identities, tenant-qualified idempotency with 409 mismatch, `entitlement_adjustment` ledger; SQLite entitlement suite 5/5 passed | `entitlements_postgres` compiled but was skipped without `MTC_TEST_POSTGRES_URL`; run it against PostgreSQL before production cutover. |
| BILL-06 | Reserve before upstream and settle actual text usage | Implemented | `reserve_usage`/`settle_usage`; balance Cucumber scenario | Add crash/retry/stream disconnect fault tests at PostgreSQL scale. |
| BILL-07 | RPM, TPM, concurrency, daily/weekly/lifetime budgets | Implemented | `KeyPolicy`, atomic rate/runtime state and budget queries; RPM Cucumber scenario | Add explicit TPM, concurrency and each budget-window E2E test. |
| BILL-08 | Model price one-click sync from multiple fixed sources | Implemented | `src/pricing.rs`, `/model-prices/sync`, operator UI | Add deterministic fixture contract tests for conflicts/source failures and deployed egress verification. |
| BILL-09 | Manual price override preserved | Implemented | sync logic and manual `source` | Add API-level regression test. |
| BILL-10 | Generation pricing/permission/reservation/settlement | Implemented | price snapshot through schema v16; Seedance, ComfyUI and image Cucumber scenarios; `generation_jobs` tests passed 5/5 | Real providers and production-scale assets remain deployment verification. |
| BILL-11 | Async generation submission retry and queued cancellation | Implemented | optional stable-key-scoped submission idempotency, request-hash conflict detection, atomic queued cancellation/refund; the 5/5 `generation_jobs` gate covers replay/refund, repeat cancellation, active-lease refusal and price snapshots | Provider-specific cancellation after upstream submission is not implemented. |
| BILL-12 | Tiered token pricing including cached input/cache write | Implemented | schema v18 tier storage and immutable snapshot; OpenAI/Anthropic cache normalization; four-dimensional/tiered HTTP response; pricing unit suite and Cucumber cache/tier scenario passed | Verify real provider usage variants and catalog drift during dogfood. |

## Unified upstreams, OAuth, routing, and plugins

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| UP-01 | API credentials and OAuth are first-class methods of one upstream resource | Implemented | one `upstream_accounts` table and `UpstreamCredential` enum; unified list/API | Keep UI and docs terminology aligned. |
| UP-02 | Stable upstream ID across credential rotation/refresh | Implemented | `rotate_upstream_credential`; API/OAuth Cucumber scenario | None for core invariant. |
| UP-03 | Unified upstream management UI, no separate OAuth tab | Implemented | one provider onboarding surface in `Operator.tsx`; Playwright verified direct credential and OAuth/subscription methods in the same tab | Re-run browser acceptance against each deployed release. |
| UP-04 | Cursor direct PKCE and refresh | Implemented | `src/oauth.rs`; Cursor Cucumber scenario | Real provider smoke test still required. |
| UP-05 | GitHub Copilot/Cursor CPA subscription bridge | Implemented | bridge start/poll/inference and Cucumber scenario | Real bridge persistence/restart and expiry tests remain. |
| UP-06 | CPA subscription account import without echoing secrets | Implemented | import endpoint, fingerprinted result, Cucumber idempotency/fail-closed scenario | Verify real export variants. |
| UP-07 | Provider route creation and priority | Implemented | idempotent create, priority validation, provider-protocol compatibility and `resolve_upstream`; route-management test passed | Multi-route failover policy remains a separate product decision. |
| UP-08 | Route listing | Implemented | tenant/global `GET /model-routes`; route-management tenant/scope tests and operator browser flow passed | Re-run against deployed PostgreSQL data. |
| UP-09 | Route update/disable/delete | Implemented | optimistic PUT/PATCH, disabled-and-unreferenced DELETE, history protection; `route_management` 1/1 and targeted operator Playwright flow passed | PostgreSQL-specific route concurrency test remains. |
| UP-10 | Plugin contributes provider config and OAuth adapter | Implemented | provider contributions, manifest schema and OAuth adapter endpoints | Add a packaged third-party example and install/upgrade E2E. |
| UP-11 | Plugin contributes traffic/rewrite policy safely | Pending verification | Wasmtime component ABI, fuel/memory/HTTP/KV bounds | Demonstrate a real policy plugin, timeout/fuel denial and deterministic fail-closed behavior. |
| UP-12 | Third-party plugin packaging/distribution | Pending wiring | local manifest/package documentation and read-only mount exist | OCI fetch/signature/trust/update lifecycle is not a completed product workflow. |
| UP-13 | Unified upstream lifecycle management | Implemented | list metadata plus optimistic PUT/PATCH, bounded health, audit-safe DELETE, and idempotent credential/OAuth refresh; `upstream_management` 2/2 and Playwright management flow passed | Re-run concurrency and real OAuth/provider probes against isolated PostgreSQL before release. |

## Client protocols and multimodal generation

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| PROTO-01 | Codex via OpenAI Responses | Implemented | `/v1/responses`, OpenAI proxy pipeline | Real Codex dogfood regression remains release evidence. |
| PROTO-02 | Claude Code via Anthropic Messages/count_tokens | Implemented | `/v1/messages*`; Claude Cucumber scenario | Real Claude Code smoke test remains. |
| PROTO-03 | Copilot/Cursor/WorkBuddy/OpenAI-compatible clients | Pending verification | `/v1/chat/completions`, embeddings and bridge route | WorkBuddy and non-bridge Cursor/Copilot real-client matrices are not present. |
| PROTO-04 | OpenAI-compatible model discovery | Implemented | `/v1/models` filters priced models through policy | Add empty policy/wildcard tests. |
| MM-01 | Seedance video generation | Implemented | Seedance driver, worker polling/archival and Cucumber scenario | Real Volcengine OAuth/API credentials and failure-code smoke test remain. |
| MM-02 | ComfyUI image/video generation | Implemented | ComfyUI driver, administrator-owned workflow validation and Cucumber scenario | Real local and cloud ComfyUI workflows need deployed verification. |
| MM-03 | OpenAI Images forwarding and billing | Implemented | `/v1/images/generations`, image Cucumber scenario | Real provider smoke test remains. |
| MM-04 | Codex Responses image tool exposed through Images API | Implemented | responses-tool transformation and Cucumber scenario | Verify through the deployed relay rather than direct upstream. |
| MM-05 | Large generated asset archival without gateway memory growth | Pending verification | worker streams bounded assets; synchronous image body/semaphore bounds | Add 100–500 MiB asset load/soak test with memory measurements. |

## Requests, statistics, conversations, and migration

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| OBS-01 | Started/finished realtime request monitoring | Implemented | PostgreSQL request events and resumable SSE API; Cucumber stream assertion | Add reconnect/cursor/retention/load verification. |
| OBS-02 | Efficient operator aggregate statistics | Functional PostgreSQL verified | default 30-day/maximum 93-day bounded statistics with time, credential, model, protocol, status, error, upstream, route, duration, cost, alias and principal filters; isolated PostgreSQL/SQLite observability suite passed 2/2 | Imported-production-size EXPLAIN/latency benchmark remains. |
| OBS-03 | Operator request/error archive detail | Implemented | bounded archive lookup with tenant/global enforcement | Add redaction policy and configurable retention acceptance. |
| OBS-04 | Self-service CPAMP-style statistics | Pending verification | self stats/requests/generations/conversations UI and APIs | Feature parity needs browser rubric/screenshots against CPAMP reference. |
| OBS-05 | Browse old/high-volume request history | Functional PostgreSQL verified | descending `(before_created_at, before_id)` cursor plus the complete operator filter set and self-service stable-key boundary; isolated PostgreSQL/SQLite observability suite passed 2/2 | Imported-production-size benchmark remains; page limits are 100/500. |
| CONV-01 | Explicit client session/turn/parent/branch/compaction metadata | Implemented | `ConversationHints`, observation/edge persistence and passing Cucumber compaction scenario | Expand real Codex/Claude metadata fixture corpus. |
| CONV-02 | Infer logical conversations without sufficient metadata | Implemented | tenant/principal-scoped semantic atoms and Merkle prefix nodes | Validate precision/recall on real imported sessions; no quality threshold is yet defined. |
| CONV-03 | Compression remains related to prior conversation | Implemented | `compacts` relation and explicit parent evidence | Add multiple consecutive compactions and branch-after-compaction fixtures. |
| CONV-04 | OpenAI Responses `previous_response_id` preserves parent relation | Implemented | schema v16 response-ID storage plus streaming/non-streaming black-box scenarios; expanded protocol feature passed 6 scenarios/36 steps | Keep real-client fixture corpus current. |
| MIG-01 | Initial CPA/CPAMP usage/alias/price import | Fixture PostgreSQL verified | isolated K3S PostgreSQL acceptance ran source/target counts, distinct IDs, token/cost totals, one error and request/error body-gap samples | This was a synthetic CPAMP SQLite fixture, not a live CPA/CPAMP snapshot; real immutable-snapshot reconciliation remains. |
| MIG-02 | Repeatable incremental import until cutover | Fixture PostgreSQL verified | the same isolated acceptance replayed the initial import, imported a delayed overlap row and a new watermark row, proved deterministic IDs, tenant/source checkpoint isolation and fail-closed unmapped behavior | Run repeated live increments behind a final CPA write barrier and prove zero omissions before traffic shift. |
| MIG-03 | Preserve original CPA client credential | Implemented | legacy credential registration/import path | Validate exact production Linux Codex credential without exposing it in logs. |
| MIG-04 | Rehydrate cpa-session-archive bodies and session observations | SQLite/CAS verified | schema-v2 importer integration 1/1 and module unit tests 2/2 cover fail-closed preflight, exact linkage, CAS apply/replay, checkpoints, observations and no-overwrite | No PostgreSQL, S3 or live session archive import has been run; execute isolated PG/bucket acceptance before live data. |

## UI, schemas, API governance, and security

| ID | Requirement | Status | Authoritative evidence | Remaining acceptance evidence/work |
|---|---|---|---|---|
| UI-01 | Chinese/English i18n switch | Implemented | `web/src/i18n.tsx`; Playwright switched Chinese operator UI to English and asserted translated tabs/headings | Extend the browser crawl as new screens are added. |
| UI-02 | Light and dark themes | Implemented | theme CSS/control; Playwright switched dark-to-light operator UI and loaded the portal in light mode at mobile width | Add screenshot visual regression if pixel-level stability becomes required. |
| UI-03 | Website icon at appropriate sizes | Implemented | icons under `web/public`; Playwright fetched the linked PNG and checked its media type/content | Recheck deployed favicon cache behavior after asset revisions. |
| UI-04 | JSON Schema-rendered simplified configuration | Pending verification | `/schemas`, RJSF operator forms and schema templates | Verify all schemas under strict CSP and that submitted values match backend validation. |
| SCHEMA-01 | Provider/plugin schemas enforced server-side | Pending wiring | provider schemas are discovered/rendered; targeted handwritten validation exists | Full JSON Schema validation is not the authoritative server validation path. |
| API-01 | Discoverable OpenAPI contract | Implemented | `openapi/openapi.yaml`; Redocly validation passes and source/OAS route-set comparison covers every registered versioned API route | Add CI lint and schema-level conformance; the service does not serve `/openapi.json`. |
| API-02 | Explicit idempotency, tenant and error contract | Pending verification | OpenAPI and `docs/api-contract.md` | Add black-box contract tests for every documented status/response. |
| API-03 | API version/deprecation metadata | Implemented | protected control `/version`, `X-MTC-API-Version: v1`, `/healthz` deprecation headers, build injection; metrics integration tests 3/3 | Define a concrete minimum deprecation duration/sunset policy before the first removal. |
| SEC-01 | Authenticate before buffering protected bodies | Pending verification | control/gateway `route_layer` authentication middleware in current source | Add oversized unauthenticated-body regression and deployed reverse-proxy verification. |
| SEC-02 | Tenant isolation/IDOR resistance | Implemented | tenant/key constraints plus passing Cucumber global/scoped/downstream authorization matrix and Playwright cross-tenant 403 assertion | Extend to new endpoints and fuzz IDs/filters. |
| SEC-03 | SQL/command/path injection protection | Pending verification | SQL binds, web asset component checks, importer sanitization | Dedicated fuzz/static security evidence still required. |
| SEC-04 | Secret-safe HTTP errors/logging | Pending verification | sanitized upstream/storage `AppError`; encrypted upstream credentials | Audit every log field and plugin/OAuth/import failure under adversarial secrets. |
| SEC-05 | Public browser security headers | Implemented | CSP, HSTS, no-store, nosniff, frame and permissions policy middleware | Recheck at ingress/CDN after every deployment. |

## Verification evidence for this snapshot

The OpenAPI document was parsed and linted with:

```bash
npx --yes @redocly/cli@2.20.0 lint openapi/openapi.yaml
```

The command exited successfully and declared the API description valid. Its only four warnings are the recommended-rule warnings that the non-erroring `/healthz`, `/livez`, `/readyz`, and fallback-capable `/portal` operations have no invented 4xx response.

The following results were supplied by the implementation/test agents for changes present in this working tree. They were completed at the points reported; this documentation task did not rerun the entire combined suite after every concurrent edit:

- On the stable v23 tree, `cargo test --all-targets` passed; the library suite passed 73 tests, the targeted DB suite 20/20, generation jobs 5/5, and the default Cucumber run 39 scenarios/218 steps. The Cucumber result includes the expanded six-scenario/36-step conversation-protocol feature.
- `npm run typecheck`, `npm run build`, and Playwright 2/2 passed, including operator and client-credential portal dogfooding.
- `cargo clippy --all-targets -- -D warnings`, `cargo fmt --all -- --check`, and the diff check passed at the reported combined-tree gate.
- Credential rotation was exercised against an isolated schema on the K3S development PostgreSQL instance and cleaned up afterward: eight concurrent client-key rotations with one idempotency key all replayed generation 2; four distinct keys serialized as generations 3–6; eight concurrent service-credential rotations replayed generation 2.
- `entitlements_postgres` ran against an isolated temporary PostgreSQL database and restricted role: 1/1 passed, then cleanup verified zero remaining test database/role objects.
- `route_management` passed 1/1 and `upstream_management` passed 2/2; their targeted operator Playwright management flows passed.
- The cache/service-tier pricing implementation passed the 62-test library gate plus its seven-step Cucumber scenario. HTTP types expose all four price dimensions and tier metadata.
- The earlier v1–v20 PostgreSQL gate used a fresh temporary database and restricted role. `postgres` passed 2/2, `entitlements_postgres` passed 1/1 and `observability_filters` passed 2/2 (one PostgreSQL and one SQLite test). `schema_migrations` reported exactly versions 1–20 with no gaps, and the request/request-event partition count was 20. Cleanup verified zero test databases/roles; the three remote `kubectl port-forward` processes created by this gate were identified by PID plus full argv and removed; its local tunnel/temp files were removed. A pre-existing, unrelated port-forward on 15432 was deliberately retained.
- `session_archive_import` passed its SQLite/CAS integration 1/1 and module unit tests 2/2. No PostgreSQL/S3 or live session archive import was performed.
- The isolated K3S PostgreSQL CPAMP acceptance passed initial import, exact replay, delayed-overlap increment, new-watermark increment, deterministic IDs, tenant/source checkpoint isolation, and fail-closed unmapped behavior. Its final synthetic fixture reconciled 4 source rows to 4 distinct target requests, 70 input tokens, 26 output tokens and 244 cost micros. This was not live CPAMP data.
- Strict Helm lint passed default and dev values. Render/schema/kubeconform gates passed the default 23-resource and canary 9-resource sets, including negative schema checks for empty enabled network rules. The fail-closed selector values were not applied to the live dogfood release.
- The second final PostgreSQL gate used a fresh restricted database and applied exactly schema migrations 1–23 (`missing=0`). The DB `postgres_*` regressions passed 3/3, `tests/postgres.rs` passed 3/3 including v22 budget concurrency and the v23 generation aggregate path, `entitlements_postgres` passed 1/1, and `observability_filters` passed 2/2. Final structural review found all eight core tables, primary keys on both locator tables, one actual locator row and 20 request/request-event partitions. Precise cleanup verified zero `mtc_gate2*`/`mtc_struct*` databases and roles, zero port-forward processes on 54246 and zero local test tunnels; the unrelated pre-existing port-forward on 15432 was retained. The gate does not provide the pending 141k-row migration lock-time or imported-scale EXPLAIN/latency evidence.
- The 500 MiB asset plus 15-minute release acceptance passed. This is a bounded release gate, not proof against longer-term leaks under production traffic.

These results do **not** replace live external OAuth/provider, production CPAMP/session-archive reconciliation, imported-data load/soak, or public-ingress/NetworkPolicy verification. In particular, passing the CPAMP PostgreSQL fixture does not imply that cpa-session-archive bodies were imported into PostgreSQL, and the live dogfood NetworkPolicy was still permissive at audit time.
