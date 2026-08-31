# Development handoff

This is the resume point for the next Token Center development agent. Read
[Product requirements](product-requirements.md) first, then
[Architecture](architecture.md). Those documents preserve the agreed product
scope and rejected designs; this document preserves the implementation state and
remaining acceptance gates, updated on 2026-08-31.

## Active 2026-08-31 release-window continuation

- Clean source `4e8d2468b61c6505c8efc3b745acf632e4d7f2d0` passed the complete
  GitHub Actions release in run
  [`33360162697`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33360162697).
  Its verified service, importer and plugin-installer digests are respectively
  `sha256:291a788e0404d99d329826c0c059cffdfb3d172cedcc824937e1461d5fa9f810`,
  `sha256:2616b2adc5b944c3fc857de9178b061aa7b6ad7c11ca5674c6fbd8c8b5a9542b`
  and
  `sha256:5d63a4b5b15b7ace3ceeb13a7267e4d299d8d5c52af382635f20a58d1ef23298`.
  API2 trial currently runs that exact service digest with gateway, control and
  worker Ready and at zero restarts. API3 and the old CPA route remain
  unchanged.
- All 10 active legacy credentials retain their stable key identities and
  history. Their balances were raised through the ordinary idempotent grant API
  to the ledger maximum of `9223372036854775807` micros, with zero reservations,
  no account budgets and an exact zero-write replay. This intentionally removes
  balance admission as a cutover restriction; it does not broaden route,
  rate-limit or concurrency policy. The owner may lower balances later.
- Live source model discovery found 13 distinct currently visible models. The
  three models whose current target prices were missing were synchronized and
  persisted, and all 13 now have durable price records. Missing future model
  prices remain fail-closed; a model is not free merely because a legacy account
  carries the maximum balance.
- A strictly API2-trial-only compatibility diagnostic attached each of the 10
  credentials to one exact old-CPA-backed account and its own model inventory.
  Exact replay created no additional resources; 10 non-streamed Responses and
  one Chat Completions request passed. Review then proved that bridge traffic
  would be represented once as a native MTC request and again by the later
  CPAMP delta, because the old source does not retain an exact MTC request ID.
  The diagnostic was stopped before any hostname cutover: all 64 routes and all
  10 accounts are disabled by CAS, active requests/reservations are zero, old
  credentials expose zero bridge models, and the exact egress removal is
  committed through GitOps. The retained 20 requests (11 success, nine failure)
  and disabled resources remain audit evidence; they must be explicitly
  reconciled rather than imported as duplicate facts. The temporary bridge tool
  is not part of the release tree. Direct provider-exact route/policy convergence
  remains the production gate.
- A real Codex CLI Responses stream then exposed a release blocker in the
  current image. The old CPA emits a valid terminal `response.completed` event
  followed by one harmless extra blank line. The streaming sanitizer forwarded
  the complete response but retained that final newline and recorded
  `upstream_incomplete_response`/HTTP 502. The local continuation accepts only a
  CR/LF-only suffix after a fully framed event; the independent terminal-event
  capture still rejects missing completion and arbitrary partial fields. The
  complete 380-test Rust library, strict library Clippy, root TypeScript
  typecheck, all runnable operations tests, all release contracts, the
  TypeScript-only repository contract and `git diff --check` pass locally.
- Do not promote the current service digest. Publish one replacement exact-SHA
  release from this locally accepted tree, deploy only that immutable
  service digest to the reversible API2 trial, and require a real Codex CLI
  request plus an exact old-CPA terminal-stream fixture to persist HTTP 200 with
  validated total/cache usage without re-enabling the bridge. Complete direct
  route, final CPAMP delta, session archive and browser reconciliation before any
  API3 change.

## Active 2026-08-29 billing-correction and Web redesign gate

- The sealed online CPAMP snapshot contains 350,631 events, has SHA-256
  `63c5f78bc96f5db7d3e02683dba2c0e66b63a2d2d420b5905dfbda2013be7898`,
  and passed SQLite `quick_check`. Job
  `mtc-cpamp-import-e339de8-r2-20260829` committed 40,782 new rows and advanced
  the checkpoint to exactly 350,631 events. Job
  `mtc-cpamp-replay-e339de8-r2-20260829` remains deliberately suspended. Do
  not resume it yet.
- Read-only reconciliation proved that the released importer treated all
  imported cache usage as ordinary prompt input: historical rows have zero
  cache-write and effectively zero cache-read Token, while 71,456 cached Token
  belong to later live trial requests. This is caused by missing normalized
  cache, resolved-model, service-tier and tiered-price fields in the CPAMP
  staging/apply contract. The resulting historical cost is not trusted for
  acceptance or production promotion.
- Ordinary importer replay cannot repair these rows because the v1 locator and
  source digest intentionally claim each event once. The required fix is a
  TypeScript-only, versioned billing correction with a dry-run, immutable
  provenance, compare-and-swap updates, tenant-scoped fact/aggregate rebuild,
  exact rollback evidence and zero-write correction/importer replays. It must
  not change balances, grants, reservations, ledger entries, non-imported live
  rows or global real-time prices. Missing price evidence fails closed.
- The source worktree is intentionally under active local development on top of
  `f2a1ad5`; no new release SHA or image digest exists yet. In parallel, the Web
  application is being split into URL-addressable Portal and Operator pages,
  with true side/mobile navigation, exact locale-grouped numbers, accessible
  ECharts analytics, session context on recent requests, and a credential-scoped
  self-service usage endpoint. Implementation-detail copy is being removed.
- Before any new CI run, complete local TypeScript/Rust/API/operations gates and
  sanitized Chromium acceptance at 320, 390, 768, 1024, 1440, 1920 and 2560
  pixels in Chinese/English and light/dark. Use one final exact-SHA GitHub
  Actions run only after the local tree is complete. Redeploy only the reversible
  API2 trial with the resulting immutable digests, re-run browser and Codex CLI
  text/image dogfood, and retain the old digest rollback. API3 remains forbidden
  until the user explicitly opens the production window.

## Current 2026-08-29 exact release status

- The immutable deployed release source is
  `e339de8982cd2b485f02e1252bf449bbd564b560`. GitHub Actions run
  [`33240090081`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33240090081)
  passed repository/dependency security, API contracts, Web Chromium, Rust
  format/Clippy/all tests, migrations, packaging, the optimized 15-minute
  memory/stream/500 MiB asset gate, all three image publications and immutable
  release verification. Release-manifest SHA-256 is
  `32a9319a0d4b28d046818b3d737d8b268bfb2b6029ccfa9f915a13b7436b8b0b`.
  The verified service, importer and plugin-installer digests are respectively
  `sha256:d4be9bcbd5b5ed5d906ea007aaeb307b2645ece4e169997506f595ca2771afb1`,
  `sha256:75fef96d76af5fb39851514fd89ba8e12fee394e88c7d6f5fac91a5e8ca5a429`
  and
  `sha256:b29b0d5da789744b07fe85d579bad54d47f8d8fa8d0aeebf13903f4c1c41c9af`.
- API2 trial runs that exact service digest at schema 59. Gateway, control and
  worker are 1/1 Ready with zero restarts. Argo is Synced/Healthy and manual
  after GitOps revisions `80eb28c` (candidate), `746b5ae` (one-shot sync) and
  `c3416f3` (automation removed). The prior service digest
  `sha256:24e46602754d80dc885959ce83f4d08040081ce93db6c4331c9c24ae93ae7e39`
  remains the immediate schema-compatible rollback point. API3 and old CPA,
  including their routes, storage and images, were not changed.
- Exact-release Chromium acceptance passed singleton tenant selection, all
  seven usage views, usable price synchronization, collapsed credential
  creation, the custom-model confirmation regression, stable dogfood key and
  44 historical rows, credential reload/manual-clear, Chinese/English,
  light/dark, 390-pixel responsiveness and gateway isolation with zero browser
  failures. The upstream selection check also waited for the exact live
  model-catalog request and rendered the selected account chip, closing the
  prior browser race without widening a timeout. The remote workspace remains
  outside the reviewed operator ingress
  source allowlist, so its public operator request returns the expected
  ingress-layer 403 and the same live control Service was tested through a
  temporary port-forward. The port-forward was stopped. See
  [exact API2 acceptance](operations/2026-08-29-api2-e339de8-live-acceptance.md).
- Codex CLI `0.150.0-alpha.8` returned exact text marker
  `MTC_API2_E339DE8_NO_RETRY_TEXT_OK` through API2 Responses with both request
  and stream retry counts set to zero. It persisted the declared session name,
  `release-dogfood-no-retry` task kind and one successful request with zero
  errors. The same CLI invoked a TypeScript Images driver: real `qwen-image`
  request `01a04ca8-6621-7330-b472-c5dfaa172c21` returned HTTP 200, settled
  0.01 USD, archived a 515,188-byte PNG and replayed twice with the same request
  ID and byte-identical 288-byte response without another provider generation
  or image charge.
- The strict TypeScript-only legacy key-policy and provider-exact route
  importers are released but have not been applied. Owner-private review
  resolves 21/23 source mappings to one account and 18/23 source coordinates;
  two account choices, five source coordinates, eight live-route relation
  reviews, two managed Codex mappings, Copilot/Cursor reauthorization and one
  malformed anomaly remain fail-closed. Do not create a strict manifest or run
  live apply until those owner decisions exist.
- Production remains blocked by that route/policy convergence, the approved
  collector-only v0.8.1 maintenance and complete session archive
  baseline/delta/apply/replay, final CPAMP fence, any separately authoritative
  subscription inventory, paired PostgreSQL/MinIO backup and external restore,
  OAuth continuity and formal route rollback. The dedicated disabled-provider
  canary is complete: its sentinel appeared zero times in live control data and
  Chromium, the ordinary client was denied, and cleanup returned the cluster
  object inventory to the exact prior digest. API3 remains prohibited until
  the user explicitly opens the production window.

## Pre-release 2026-08-29 convergence history

This retained section describes the immediately preceding `413f151` baseline
and the then-uncommitted convergence tree. It no longer describes the current
release; the exact status above supersedes it. Its rehearsal and migration
evidence remains valid.

- Clean `master` and `origin/master` released baseline is
  `413f15161397f3ac640b4921c8e7becdfc8aa3b9`. GitHub Actions run
  [`33216849486`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33216849486)
  passed the complete exact-SHA release, including 35/35 memory checks. The
  immutable service, importer, and plugin-installer digests are respectively
  `sha256:f059036b27f2543972e2b5edcd273aea4ff56e46c094e7b330c9dde0d0de369c`,
  `sha256:cdfd7a8a14be6b1402cd126542a5166ae1843d982e1d98730c4352ccb0c6cd90`,
  and
  `sha256:9bdded96e1bc8a9492239d87f0fce1d8ffd73f3f7bd5ff4c7188fc2bb99cc625`.
- API2 gateway, control, and worker run the exact service digest above at
  schema 59 and are each 1/1 Ready. The low-lock live index inventory has 14/14
  attached request leaves, the generation index is valid/ready, and no rows
  moved. The pre-v59 PostgreSQL dump is retained on the dedicated trial PVC,
  but it is still a same-cluster PostgreSQL-only rollback point rather than a
  paired external PostgreSQL/MinIO recovery proof.
- Playwright Chromium 151 re-accepted the deployed API2 portal and control
  Service on 2026-08-29. It proved the stable client key, 44 visible historical
  rows, remember/manual-clear semantics, seven usage views, Chinese/English,
  light/dark, zero 390-pixel horizontal overflow, and gateway 200/404 routing
  isolation. The exact read-only receipt is
  [API2 v59 live browser acceptance](operations/2026-08-29-api2-v59-live-browser.md).
- Current uncommitted source changes on top of `413f151` fix operator stale
  scope restoration after explicit credential clear while preserving a valid
  session after a failed direct replacement, preserve generation drafts across
  background refresh, remove two browser scheduling races, and make the
  imported-scale conversation EXPLAIN gate distinguish bounded empty/tiny
  partition scans from real bulk scans while independently requiring every
  conversation leaf index valid/ready. They also add the TypeScript-only
  legacy policy importer, provider-exact route convergence importer,
  importer-image integration and two hardened default dry-run Jobs. The policy
  importer pins exact source-pattern to route/account/source
  pairs, live route topology and grant CAS; unknown, unmapped, anomalous or
  broader grants fail closed, partial progress is checkpointed, and exact
  replay writes nothing. The route importer separately requires all source
  mappings, one exact upstream candidate per route, owner-selected priorities,
  checkpoint-prefix live revalidation and zero-write lost-response replay; it
  rejects generation expansion, relations on an update, or a target inventory
  at the 100-row single-page boundary.
- After rebuilding `web/dist`, Web build/typecheck and real Chromium passed
  20/20 scenarios and 145/145 steps. Root TypeScript typecheck and the combined
  operations suite passed 59 runnable tests with three explicit environment
  skips. The
  conversation live snapshot gate passed on 309,893 requests and 17
  projections with zero missing indexes; list, membership, first-page, and
  older-page execution times were 0.239, 0.112, 0.378, and 0.402 ms, with no
  bulk sequential scan. The sanitized report is retained at
  [`docs/evidence/api2-v59-conversation-explain-summary.json`](evidence/api2-v59-conversation-explain-summary.json).
  These changes have not triggered another CI run or produced replacement
  images yet.
- A secret-safe live continuity probe exercised all 10 active legacy CPA
  credentials through the public API2 gateway. All 10 authenticated, all 10
  resolved to distinct stable key identities, all 10 could read their own
  history (309,849 imported requests in total), and all 10 were denied by the
  control plane. However, all 10 returned an empty `/v1/models` list. The
  authoritative old policy has 10 enabled, non-uniform entries with 2–36
  grants per credential, while API2 currently has only 10 reviewed routes and
  lacks several old source families. This is a P0 routing-continuity failure,
  not a credential-attachment failure. The count-only receipt is
  [`docs/evidence/api2-v59-legacy-credential-continuity-summary.json`](evidence/api2-v59-legacy-credential-continuity-summary.json).
- A second read-only audit corrected the earlier ambiguous “27 upstreams”
  wording: the old source has four provider blocks and 23 nested model
  mappings, but seven API-key connections plus two managed Codex OAuth
  connections. Those nine connections match the nine current API2 upstreams.
  Routing still does not: both managed Codex accounts have zero route
  candidates, Copilot and Cursor each need native reauthorization, Claude is
  absent, and DeepSeek/GLM/Qwen mappings are compressed across provider
  boundaries. The exact count-only matrix is
  [`docs/evidence/api2-v59-upstream-continuity-summary.json`](evidence/api2-v59-upstream-continuity-summary.json).
- The sealed CPAMP inventory has 37 tables and no durable user, paid
  subscription, entitlement, tenant, principal, billing or plan table. Two
  subscription-labelled Copilot/Cursor entries are upstream OAuth connection
  state that requires reauthorization, not customer credit. Do not infer paid
  entitlements from aliases, usage identities or upstream accounts; use a
  separately authoritative MemeLoop Web source if one exists.
- A 2026-08-29 read-only session-archive preflight fenced the still-Ready
  v0.7.21 source at 3,826 sessions, 329,961 records, 1,098,909 blobs and
  17,697,944,272 compressed bytes. Although the immutable importer contains
  the compatible exporter, the legacy source returned only 100 sessions
  (37,928 projected requests) even when asked for 10,000 and advertises neither
  a stable cursor nor an offline-full snapshot. The exporter must therefore
  fail closed rather than omit data. API2 archive tables remain empty; the
  check wrote neither PostgreSQL nor MinIO and removed all four temporary
  inspection Pods. The sanitized receipt is
  [`docs/evidence/api2-v59-session-archive-preflight-summary.json`](evidence/api2-v59-session-archive-preflight-summary.json).
- Remaining production blockers are creation of provider-exact missing routes,
  Copilot/Cursor reauthorization, an owner-reviewed old-policy to exact-route
  mapping plus live dry-run/apply/replay, the complete
  approved collector-only v0.8.1 maintenance, fresh isolated 100 GiB source
  clone and separate 100 GiB evidence volume, measured archive capacity and
  baseline/deltas/apply/replay/reconciliation, final CPAMP
  delta and source fence, a separately authoritative subscription
  inventory/reconciliation if one exists, a paired
  PostgreSQL/MinIO backup plus external restore drill, formal route rollback,
  and the exact multi-account route/policy convergence. The dedicated live
  disabled-provider secret canary was subsequently completed and cleaned up;
  see the exact-release acceptance receipt. API3 is unchanged and prohibited
  until the user explicitly opens the production window.

## Current schema-v59 release contract

This section supersedes only older statements that call v58 the current
working-tree or fresh-release schema. It does not rewrite the retained v55→v58
migration evidence or the live API2 v58 state below. The isolated rehearsal
recorded immediately below is v59 PostgreSQL evidence, but it is not a v59 API2
deployment.

- The working tree now registers schema v59 for PostgreSQL and SQLite. It adds
  ordered `(model, created_at DESC, id DESC)` request-history and
  `(public_model, created_at DESC, id DESC)` generation-history indexes so the
  all-tenant model filter can take a bounded Top-N from each source before its
  merge.
- The release Helm values and migration Job annotation require v59 and the
  packaging contract rejects either backend/version drift or an override back
  to v58. Application Pods must remain behind the migration gate.
- On 2026-08-28 UTC, the actual current migration binary applied a brand-new
  PostgreSQL 17 database from v1 through v59. `schema_migrations` contained
  exactly 59 distinct contiguous versions, both v59 parent indexes were
  valid/ready, and all ten fresh request-partition leaf indexes were attached
  and valid/ready. The fixed-name temporary database and port-forward were
  removed after verification. This closes the fresh-sequence gate but is not a
  live API2 migration or rollout.
- PostgreSQL installs the v59 partitioned parent and generation indexes inside
  the transaction migration, which can take write-conflicting locks on live
  history. Before rollout, rehearse the real snapshot and either hold a measured
  write barrier or use the reviewed low-lock prebuild procedure to build and
  verify every leaf plus the generation index before applying v59. Do not raise
  migration deadlines to hide this risk, and do not roll any v59 writer until
  the migration and index inventory both succeed.

### Isolated API2-snapshot v58→v59 rehearsal

- On 2026-08-28 UTC, an exact logical clone of the live API2 database was made
  in the same PostgreSQL 17 cluster. Source and clone both contained 309,888
  request records and one generation job; the clone started at schema v58 with
  14 request partitions and neither v59 index. The live source database was
  only read by `pg_dump` and was never migrated or indexed.
- Before v59, the real all-tenant `deepseek` request branch took 20,211.210 ms,
  hit/read 271,979 shared blocks and filtered 269,797 non-matching rows while
  walking the old time indexes. The default partition alone filtered 220,016
  rows.
- The TypeScript low-lock operator first failed closed because PostgreSQL 17's
  column form of `pg_get_indexdef` omits sort direction. Its metadata check now
  combines the column expression with `pg_index.indoption`, including
  non-default NULL ordering, and the static contract pins that behavior.
- After that fix, `--apply --indexes-only` created every request leaf with
  `CREATE INDEX CONCURRENTLY`, attached all 14 leaves to the `ON ONLY` parent,
  built the generation index concurrently, and verified every parent, leaf and
  standalone index as valid and ready. It moved no rows and left
  `schema_migrations` at 58. End-to-end operator wall time was 9m46s, dominated
  by more than 100 `kubectl exec` verification round trips; this is not stated
  as a database lock duration.
- The exact post-index branch used index-only scans on all 14 leaves and fell
  from 20,211.210 ms to 5.567 ms. The complete read-only imported-scale
  benchmark passed every 250 ms and sequential-scan gate: global model request
  Top-N was 84.259 ms, global newest was 128.824 ms, all required model indexes
  were valid/ready and unattached leaves were zero.
- The actual current migration binary then applied v59 over the prebuilt
  indexes and recorded exactly one schema row at version 59; both model indexes
  remained valid/ready, all 14 leaves remained attached, and the request count
  remained 309,888. The fixed-name clone, temporary binary, wrapper, benchmark
  file and port-forward were removed afterward. API2 remained schema v58 and
  its gateway/control/worker Deployments stayed 1/1 ready; API3 was untouched.
- Remaining release evidence is the unchanged final memory gate/CI and live
  same-digest API2 re-acceptance. Writer-contention
  timing on a larger production snapshot remains an operations observation;
  the reviewed rollout path is the verified concurrent prebuild, not a longer
  blocking migration timeout.

## Current 2026-08-28 v59 working-tree convergence

This section supersedes the local-source and release-next-step claims below.
`master` and `origin/master` both point to clean released baseline
`0cf5fdc2b8a8dcb50042c9f6c0893cd5f67566cc`; the v59/UI/security work described
here is currently an uncommitted working tree on top of that baseline. API3
remains unchanged and prohibited until the user explicitly opens the production
window.

- Schema v59 and both backend migrations add the ordered all-tenant model
  history indexes. The request-list query takes a bounded Top-N from request
  and generation history before merging. The TypeScript low-lock operator uses
  `ON ONLY`, per-leaf `CREATE INDEX CONCURRENTLY`, partition-index attachment,
  valid/ready checks and no forged migration row.
- An isolated API2 snapshot clone with 309,888 requests passed concurrent
  prebuild, actual v58→v59 migration and the full 250 ms plan gate; the real
  `deepseek` branch improved from 20,211.210 ms to 5.567 ms. A separate fresh
  PostgreSQL 17 database applied exactly v1→v59, with both parent indexes and
  all ten fresh leaf indexes valid/ready. Both temporary databases and their
  port-forwards were deleted; live API2 remains v58.
- The operator now resolves tenants before any tenant-scoped request, selects a
  singleton tenant immediately, keeps multi-tenant all-scope read-only, blocks
  stale credential races, preserves the last valid session after a failed
  replacement and keeps same-scope provider success notices through refresh.
  The ordinary-client denial test explicitly clears the remembered operator
  credential first, matching the required remember/manual-clear semantics.
- Live read-only browser auditing now permits only a predeclared exact 4xx
  navigation error. It records an HMAC URL fingerprint rather than raw browser
  error text, paths, queries or credentials; undeclared console, page, request
  and cross-origin failures still fail closed. Contract canaries prove the
  evidence path cannot retain their input.
- Local root TypeScript typecheck, 31/31 runnable operations tests (three
  explicit environment skips), seven release contracts, repository
  TypeScript-only enforcement and `git diff --check` pass. Web typecheck,
  34/34 localization/security contracts, production build and real Chromium
  20/20 scenarios with 145/145 steps pass. The targeted Rust global-model query
  and SQLite plan tests pass 2/2. The unchanged final 15-minute optimized memory
  gate and one exact-SHA GitHub Actions release are still required after commit;
  do not raise thresholds or run exploratory CI.
- No approved private, versioned external backup target or credentials are
  recorded yet. Public GHCR is only for public program images and must not hold
  private database/archive bytes. Production migration still requires the
  reviewed rollback point and writer barrier/concurrent-prebuild sequence.
- The Codex remote-browser limitation is tracked at
  <https://github.com/openai/codex/issues/34263#issuecomment-5457329806>:
  remote CLI tasks receive Desktop Tab URL/title context but no browser-control
  tool, so user-visible Tab operations cannot be claimed from that environment.

## Current deployed API2 baseline

- Clean source SHA `0cf5fdc2b8a8dcb50042c9f6c0893cd5f67566cc` passed every
  release gate in GitHub Actions run
  [`33198305605`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33198305605).
  Its complete immutable release is service
  `sha256:7dae1569d0a796120223083711e765b665b3b84ec1c4a00c53f51ea754f4c086`,
  importer
  `sha256:79ed56f37c6b7b4916fe981698efa8f904db0b79b48b03eac24b4c6fa6451636`
  and plugin-installer
  `sha256:24c747716b6730021ceb935c334c04e45d9c696c1163269e78b1b8df1e4a88bd`.
- GitOps commit `a256f8d223fd8d66b48e8b739f38df5bb2daa753` pins the service
  and plugin digests above. Read-only verification on 2026-08-28 found API2
  gateway/control/worker all 1/1 ready on the service digest and the live
  database still at schema v58. The new v59 working tree has not been deployed.
- The trial URLs remain
  `https://token-center-api2-trial-portal.k3s.onetwo.website/portal` and
  `https://token-center-api2-trial.k3s.onetwo.website/operator`. The control
  ingress is restricted to three exact Tailnet source addresses; the current
  remote development workspace can load the HTML but its internal control API
  receives ingress-layer `RBAC: access denied`. Do not misreport that as an
  application credential failure or a completed live browser pass.
- From the current remote workspace, real Playwright Chromium reached the
  public portal with HTTP 200, authenticated the dogfood client, matched its
  stable key ID, rendered 39 historical rows, restored the remembered session
  after reload without reflecting the credential into the password input, and
  removed it after explicit manual clear. A credential-free screenshot is
  retained in the task visualization directory.
- Codex CLI 0.150.0-alpha.8 then used the deployed API2 Responses endpoint with
  model `Qwen` and returned the exact requested text `MTC_API2_TEXT_OK`. The
  public self API subsequently exposed the supplied explicit session ID
  `01a04b00-0000-7000-8000-000000000001` with one request and no candidate
  edge, proving the semantic headers reached the deployed persistence path.
- API3 is still outside the production window and has not been changed.

## Retained 2026-08-28 live API2 acceptance history

The evidence below records earlier same-day candidate dogfood. It is retained
for regression context and does not supersede the current `0cf5fdc` baseline or
the v59 working-tree status above.

- Clean master `a381be00d7f77dac236334e06ebe2900566cb34a` passed GitHub
  Actions run
  [`33153204682`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33153204682),
  including all functional, migration, browser, security, packaging and 35/35
  memory checks. The complete immutable release is service
  `sha256:acabfe144a5b953767fbb42fb6de0223abd21d4d9daa6d396518f65c0cc8bc75`,
  importer
  `sha256:1c1d30cfc8e47bc35a6e52731b8102a9d3febdac5aaf18b5b967d234a8135ff3`
  and plugin-installer
  `sha256:a2369993327ca2faceb4c8c65200a001053cc4ca999a1f73d1d1611a2b61316d`.
- The reversible API2 trial is migrated to schema v58 and runs the exact service
  digest above in gateway, control and worker with one ready replica and zero
  restarts each. A pre-v58 PostgreSQL backup remains on the dedicated
  `mtc-api2-pre-v58-backup-fb29097` PVC; its dump is 107,885,667 bytes with
  SHA-256
  `ce51853143fcd653d7f79a5cf98dd177b48cde7b095d3757cbd5643a7289536b`.
- Real Chromium acceptance selected the imported tenant and opened all nine
  operator resource tabs without panel or console errors. Price synchronization
  was enabled for the explicit tenant, credential creation was collapsed by
  default, client versus bootstrap service credentials were distinguished,
  credential reload/manual-clear semantics passed, and a 375-pixel viewport had
  no horizontal overflow.
- Codex CLI 0.150.0 used the API2 Responses endpoint through a custom provider
  and returned exact requested text from both `Qwen` and `deepseek`. The
  separately exposed `qwen3-coder` route correctly failed closed because it has
  no price in the key currency; do not invent a price for that unmatched route.
  The streamed terminal events reported valid usage but used the Responses
  `auto` tier alias while Codex admitted `default`, so the live a381be0 request
  audit conservatively recorded `upstream_invalid_usage`. The current source
  fixes only the official `default`/omitted-to-`auto` alias and normalizes billing
  back to the admitted default price. Other tier mismatches still fail closed.
- A real `qwen-image` request returned HTTP 200, settled exactly 10,000 USD
  micros, archived a 596,247-byte PNG and returned only an internal Token Center
  asset URL. Exact idempotent replay preserved the request ID, response length
  and response bytes without a second charge. The archived bytes have SHA-256
  `8a18716bf635073f7bdde41f91293df0a53cc05af5ee8a7cc6d0a281beba3527`.
  The a381be0 bounded generic-MIME classifier was verified live as `image/png`,
  `asset-0.png`, exact Content-Length and byte-identical replay; it does not
  buffer the asset or weaken origin/size controls.
- The service-tier alias fix adds no migration and changes no image, archive,
  routing or authorization surface. It passed its streaming settlement
  integration test, all 374 library tests, strict library Clippy, TypeScript
  typecheck and all seven release contracts locally. One exact-SHA CI and API2
  immutable-digest recheck are still required before this fix supersedes the
  a381be0 candidate.

## Current 2026-08-28 local convergence evidence

This section supersedes older local-development status below. It does not yet
authorize a rollout: one exact-SHA GitHub Actions release must still pass and
publish the complete immutable three-image set before API2 can be updated. API3
remains forbidden until the user explicitly opens the production window.

- This convergence commit is based on clean remote-parity master
  `aae6d0fbd651251036a2c1588f84f95fb881f74e`. No GitHub Actions run for the
  continuation existed when this local evidence was recorded.
- Schema generations v56-v58 implement the archive schema-v2 stable snapshot,
  tombstone, immutable quarantine-version and bounded staging contracts. Exact,
  unlinked and quarantine reconciliation, semantic projection changes,
  tombstones, legacy and stable checkpoints, and staging cleanup are committed
  in one target transaction. The importer seals the same input and manifest file
  descriptors, verifies size plus SHA-256/BLAKE3, enforces the delta chain and
  offline baseline, handles replacement/move/delete-recreate lifecycles, and
  processes up to the one-million-session protocol limit in bounded batches.
- An isolated PostgreSQL 17 schema-only copy of the live API2 v55 database was
  migrated through v58. The real PostgreSQL archive advisory-lock and locator
  CAS integration gate passed. The temporary database and port-forward were
  removed afterward; the live API2 database remained read-only, and its legacy
  exact, unlinked and quarantine blocker counts were all zero.
- All repository-owned automation and operational scripts are TypeScript on the
  fixed Node.js 24 runtime. No tracked Python, shell or CommonJS script remains;
  importer packaging builds seven ESM entries plus their SQL assets. The root
  operator suite passed 22 tests with three explicit environment-only skips, and
  the targeted exporter, CPA transport, importer-image, legacy and memory
  contract suites passed 36/36 before the final GHCR workflow-contract rerun.
- Fixed Rust 1.95 passed `cargo check --locked --all-targets --all-features`,
  strict all-target/all-feature Clippy and the complete test suite: 386 library
  tests, all integration binaries, and 70/70 Cucumber scenarios with 379/379
  steps. OpenAPI tests passed 18/18 with 106 paths and 126 operations. Web
  typecheck, production build and localization/contracts passed; real Chromium
  passed all 20/20 scenarios and 145/145 steps. The browser run exposed and
  verified the fix for an optional empty `video_models` schema blocking ordinary
  HTTP upstream edits while the enabled SiliconFlow driver remains fail-closed.
- A fixed-toolchain optimized binary passed the unchanged full memory acceptance
  profile, 35/35 checks. The 15-minute soak had zero request failures, a 0.038
  MiB/min RSS slope against the 2.0 ceiling and a 9.148 MiB retained delta. Twelve
  concurrent 16 MiB streams used a 56.687 MiB gateway RSS delta; standard Images
  and Codex Responses-tool images used 88.894 and 57.332 MiB respectively against
  the unchanged 128 MiB ceiling. A 500 MiB asset was completely archived and
  downloaded, and peak gateway RSS was 124.883 MiB against the 224 MiB deployment
  budget. The binary SHA-256 remained
  `04b0718d80cd4412715b2c5559651877cf6a198e6901c9f2bc3910ea09718dd7`.
- The final TypeScript GHCR publication/verification review passed locally.
  Both publishing jobs install fixed Node.js 24.18.0 before running repository
  logic. Tested TypeScript binds each SHA tag to its build digest, validates the
  BuildKit SBOM/provenance and OCI source/revision labels, rejects incomplete or
  tampered evidence, verifies the complete three-image set by immutable digest,
  and writes one no-overwrite release manifest. Typecheck, 21/21 runnable ops
  tests, four focused release/language contracts, structured workflow policy and
  `git diff --check` passed; three Docker/PostgreSQL environment tests remain
  explicit CI-required skips locally.
- The first exact continuation SHA
  `795c791b4cd6129942892cbf356413dccc3b0974` was pushed once and correctly
  blocked publication in GitHub Actions run
  [`33139376566`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33139376566).
  Packaging found that the malicious-owner policy fixture itself contained the
  complete retired repository literal, which a clean tracked-snapshot scan
  rejected; dependency security independently found that `chacha20` 0.10.1 had
  just been yanked. The doomed run was cancelled instead of consuming the Rust,
  browser and memory minutes, and it must not be rerun or used as release
  evidence. The fixture now constructs its negative-test value without storing
  the forbidden literal, and the lockfile selects Rust-1.95-compatible
  `chacha20` 0.10.2. Fixed cargo-deny 0.20.2 reports advisories, bans, licenses
  and sources all green; fixed-toolchain check, strict Clippy, 386 library tests,
  every integration binary and 70/70 Cucumber scenarios with 379/379 steps pass
  after the update.
- Exact follow-up SHA `616156352ae6f6c7b47f1d8bf8e6824444e72e0d`
  was pushed only after those local gates passed. GitHub Actions run
  [`33140294297`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33140294297)
  confirmed the repository-security and API-contract fixes, then packaging
  stopped in the importer image runtime contract: esbuild had embedded the
  `yaml` package's CommonJS Node export in an ESM executable, which failed with
  `Dynamic require of "process" is not supported`. Publication was skipped and
  the remaining jobs were cancelled immediately to conserve CI minutes; this
  run is not release evidence.
- The importer build now aliases `yaml` to the package's maintained browser ESM
  entry and the always-runnable contract rejects dynamic CommonJS bridges and
  executes the resulting upstream importer. An isolated privileged DinD pod in
  the temporary `mtc-ci-preflight-6161563` namespace built the exact importer
  Dockerfile from the tracked source snapshot and passed both full image
  contracts, including the real fixture dry-run, non-root user, read-only root
  filesystem, dropped capabilities, production-only assets and SQL equality.
  That run also exposed and fixed a test-only ownership-order bug: nested files
  are now secured before their parent directory becomes owner-only. The
  temporary namespace, pod, images, volumes and local transfer archive were all
  deleted; no application namespace, ingress, PVC or API3 resource was changed.
- Native-ESM fix SHA `eb1df8ed1664175b26e106059668f7149551c580`
  reached GitHub Actions run
  [`33141674889`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33141674889).
  The previously failing importer image contract passed. The CPAMP PostgreSQL
  acceptance also reported PASS for its initial, overlap, incremental and replay
  cases, but the job was marked failed afterward because its trap restored write
  permission only on the bundle root while the deliberately read-only nested SQL
  directories remained mode 0555. The run was cancelled immediately, before
  memory or publication, to conserve minutes. Cleanup now recursively restores
  owner write permission within the validated runner-temporary directory before
  removing it; the release contract pins this exact cleanup invariant.
- The next gate is one follow-up GitHub Actions run for the exact cleanup fix
  commit. Do not spend CI minutes on an intermediate revision and do not tag
  archive v0.8.0 before that exact MTC release is green.

## Current 2026-08-27 API2 trial evidence

This section supersedes the older release-truth snapshots below.

- Clean master `495733326a72ff6b97cafa4316f91bdf7eb1508f` passed
  GitHub Actions run
  [`33085491007`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33085491007),
  including every functional, migration, browser, packaging, security, image
  scan, SBOM/provenance and immutable-release verification gate. The unchanged
  15-minute memory gate completed 18,013 requests with zero failures, a 1.936
  MiB/min RSS slope and 22.653 MiB retained delta. It published verified
  immutable digests: service
  `sha256:17ffc25076d43beea236de3fe7d0af37bd677a90e1e2b50150af43d5aed1b3e4`,
  importer
  `sha256:996a51702ba1231f45222f0327942ce616beaa9cda7384b8f67821a0eda5e37a`
  and plugin-installer
  `sha256:fe88a7eb1994ab577ce1d5ac01b5bcb6647234806880aa7940e55337d8155ec1`.
  Do not promote these interim digests: post-run dogfood exposed the worker
  scheduling defect described below, whose replacement SHA must pass the same
  release workflow.
- Clean master parent `3f648975892e61751bbda44b47053b82a134dff9` passed
  GitHub Actions run
  [`33065339748`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33065339748),
  including the optimized 15-minute memory gate, and published verified
  immutable digests: service
  `sha256:15398a1d0a722dc38c7cfdc8860260e0aa880020eaea54e8d00fbbc009f3ce5f`,
  importer
  `sha256:d6f1e1bbeb5a7b6d9d78e3e5712be1ce9b8b606d911db364248e792c9da7a25f`
  and plugin-installer
  `sha256:849f3f8ae1d20fbb418c535254616b96e8af933bb970d2d3de9d8864adcf22a0`.
- Those digests are deployed only to the reversible API2 trial. Control,
  gateway and worker report the exact source revision and remain Ready with no
  restarts. The old CPA rollback point and API3 remain untouched.
- Real Chat Completions and Responses requests returned the requested exact
  marker strings. A real Codex CLI request used the Responses wire protocol and
  captured controlled session name, task kind, agent ID and labels in the
  session/request structure. The current dynamic subagent/parent relationship
  still depends on applications sending the documented metadata headers.
- A live standard Images request exposed two configuration defects: `/v1` API
  bases were receiving a duplicated `/v1` path, and the built-in `http-json`
  schema rejected the exact generated-asset origin allowlist already enforced
  by the archive layer. The current source change fixes both and extends the
  versioned CPA transport policy with exact `result_origins_by_base_url`.
  Targeted Rust tests, TypeScript typecheck and all ten CPA importer tests pass
  locally. Do not publish another candidate until the remaining local gates are
  green.
- Follow-up CI run
  [`33077065478`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/33077065478)
  passed every functional, migration, security, packaging and browser job but
  correctly blocked publication because the 15-minute soak slope was 2.038
  MiB/min against the unchanged 2.000 MiB/min ceiling. The same evidence showed
  18,016/18,016 successful requests, 139.648 MiB lifetime gateway high-water,
  24.227 MiB retained delta and every other memory check passing. The follow-up
  removes a newly introduced full URL parse from every inference request while
  retaining the `/v1` de-duplication contract; it does not raise or round the
  threshold. Its URL/proxy tests and strict Clippy pass locally. Do not use the
  failed run as a release source; its GHCR jobs were skipped.
- A client-disconnected synchronous image execution proved that the database
  correctly made an expired image lease eligible for orphan settlement, but
  the worker invoked that settlement only from the six-hour partition
  maintenance timer. This could leave a pending request and reserved balance
  stranded for up to six hours. The current change gives orphan settlement its
  own five-minute timer while preserving the 30-minute age cutoff, active-lease
  exclusion and 100-row batch bound. All six synchronous-image recovery tests,
  the orphan-settlement test, formatting and strict library Clippy pass locally.
  The live stale row must become a terminal `request_expired` record with a
  released reservation after the replacement worker starts; do not repair it
  manually before that rollout proof.
- After applying the exact reviewed asset origin to the trial, a real image
  completed with HTTP 200, produced one 591,296-byte archived asset, replayed
  the same idempotency key without a second generation, and settled exactly
  USD 0.01. The request and result staging attempts are both bound. No wildcard
  origin or private-network relaxation was used.
- Credential acquisition and the distinction between service and client
  credentials are recorded in
  [API2 trial dogfood access](operations/api2-trial-dogfood.md). The browser must
  remember either credential until the user manually clears it.
- Legacy CPA credential attachment, complete session-archive delta import,
  failed OAuth/API upstream remediation, browser UI acceptance and final
  rollback rehearsal remain release gates. API3 is forbidden until the user
  explicitly opens the production window.

## Current 2026-08-27 release truth

- GitHub Actions run
  [`32947649161`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/32947649161)
  passed for clean master SHA
  `312adf5289fea7cf2bc9a8e3548f7dcc07f0f30a`, including publication and
  verification. Its immutable digests are service
  `sha256:ab6f0ce0288b6322af97565d3bb84eb873ab85fa26b2e5e21e0633b2bd574003`,
  importer
  `sha256:ed6cdbec0f9a0820193be14a47fabf81f331a8297474abfbded19702e7f3d022`
  and plugin-installer
  `sha256:66da0edbe7aa56cd5bf6a1cbd70ba1aa5f0c329c2e71e1ed8c6304c5c735359f`.
- Those digests are not a completed trial. The live source dry-run proved the
  Westlake upstream target is private while the importer emitted
  `network_scope: public`; apply correctly stopped and its temporary migration
  credential was revoked. The current continuation adds a separate strict
  owner-only version-1 transport policy for exact private target base URLs.
  Scope is not inferred from proxy presence, but each approved private target
  must carry a private local-DNS SOCKS5 proxy before any target request. URLs
  remain write-only, the server still revalidates DNS/IP and global authority,
  and changing scope on replay conflicts instead of duplicating an account.
- External probes of the claimed trial operator URL alternated between HTTP 403
  and 200 over six consecutive requests. No upstream apply/exact replay,
  archive apply/exact replay, stable three-role ingress, live browser dogfood or
  Codex CLI text/image evidence has been supplied. Therefore the operations
  task's broad “complete” statement is rejected as release evidence.
- The private-target continuation is local until its TypeScript, image and Job
  contracts pass and one new exact-SHA CI publishes replacement immutable
  digests. Conserve CI by completing every local gate before that single push.
  API3 remains forbidden.

## Active 2026-08-24 release override

- The production deployment window remains closed on 2026-08-24. A reversible
  CPA/API2 trial is allowed, but API3 changes are explicitly forbidden until
  the user declares the next production window open. Trial success prepares a
  release; it does not implicitly open that window.
- API3 is the production target. Deploy one exact SHA and the same immutable
  service/importer/plugin-installer digests to CPA/API2 first.
- Record the old CPA image, data backup and route configuration before the
  trial; they are the immediate rollback point. Do not mutate API3 during trial.
- The release agent must personally complete browser validation of existing CPA
  accounts/subscriptions/history, unified upstream/OAuth, provider/route/
  credential groups, credentials, price synchronization, usage/session views,
  light/dark, Chinese/English, archives and multimodal accounting.
- The release agent must also send real text and image requests through the
  CPA/API2 trial endpoint with Codex CLI.
- Only after all evidence is green and the user explicitly opens the production
  window may the same digests be released to API3. This order and window gate
  supersede every API3-first trial instruction later in this file.

## Repository and safety boundary

- Canonical repository: private `memeloop-online/memeloop-token-center` on
  GitHub.
- Default branch: `master`.
- Canonical remote `master` at convergence start:
  `bd9d2e251ece8bbe610998a3d7bb91752d30c84a`.
- Integration branch: `fix/completion-p0-p1` in
  `/home/token-center-dev/worktrees/completion-p0-p1`.
- GitHub Actions and GHCR are the only maintained release path. There is no
  Forgejo mirror, Forgejo Actions or Token Center Harbor publisher.
- All repository automation and operational helper scripts must be TypeScript
  on the exact Node.js 24 runtime. Remove tracked Python, shell and CommonJS
  scripts, their shebangs/subprocess launchers, Python-only test harnesses and
  runtime dependencies while preserving CLI contracts and security properties.
  GitHub Actions `run` blocks are runner orchestration only; repository logic is
  called through reviewed TypeScript entry points.
- Do not build on the Westlake physical root disk. Source, Cargo state, temporary
  files and build cache belong on the Longhorn-backed Coder workspace.
- Coordinate Kubernetes, GitOps, migration execution, storage cleanup, rollout
  and cluster validation with
  `codex://threads/01a00a1e-3a18-7b82-8a36-a663c0ab6adc`; always record target,
  impact, read-only baseline and rollback first.
- The CPA/API2 trial rollout is authorized under the active override above, but
  irreversible migration barriers and removal of the old CPA rollback point are
  not. API3 remains unchanged until both trial approval and a later explicit
  production-window declaration.

No secret, client credential, service token or OAuth material belongs in this
document, commits, logs or test fixtures.

## Historical convergence commits

The following commits are the source changes that must remain in the integration
history:

| Commit | Purpose | Evidence |
| --- | --- | --- |
| `62cab854d7636f1b542797bd97169531ceedc9e3` | Stream synchronous Codex Responses image response segments instead of retaining multiple 15 MiB copies. | Strict Clippy/check and targeted tests passed. Exact short memory harness passed: idle 30.797 MiB, image delta 65.074 MiB, image peak 95.871 MiB, 2/2 responses, 15,379,225 bytes each. |
| `37a50fe0d9a2709c036f02deea961bf2ebf0a9a3` | Source commit for bounded standard OpenAI Images `b64_json` parsing and segmented response/archive. | Format, all-target/all-feature check, 11/11 focused library tests and harness syntax passed. |
| `9eaf4af` | Cherry-pick of `37a50fe` onto the integration branch. | Tree verified identical to the source commit; full memory acceptance has not run. |

The short memory report for `62cab85` is available only as ephemeral Coder
evidence at `/tmp/mtc-memory-short-62cab85.json`; its key measurements are
recorded above so the handoff does not depend on that temporary file. The tested
release binary SHA-256 was recorded in that report. New acceptance evidence must
always record the exact source and complete binary/image digest.

## CI and verification state

GitHub Actions run `32671474947` tested clean source
`69a23aa4d8e72669b97904e9ecc70dce750f7f1b`. Every pre-publication job passed,
including Rust, web/E2E, API contracts, migrations, dependency and repository
security, packaging, and the formal 15-minute memory acceptance. The exact-SHA
memory report passed all functional and resource gates: stream RSS delta
50.891 MiB, synchronous-image delta 77.637 MiB, 500 MiB asset gateway delta
0.223 MiB, soak failures zero, soak slope 0.863 MiB/hour, retained delta
15.395 MiB, and process high-water mark 108.332 MiB.

Publication did not complete. The Debian-based importer candidate
`sha256:cad3c27966324bc92580443fda501648ef9c7497d16f69e12c5202f090f52ea9`
failed its HIGH/CRITICAL Trivy gate, so the service and plugin-installer jobs
were cancelled to conserve CI time and no combined release manifest exists.
That partial digest is forbidden for deployment. The importer runtime is now
pinned to Alpine 3.23.5; equivalent-rootfs validation reported zero
HIGH/CRITICAL findings while preserving Node.js 24, PostgreSQL 18 and SQLite
operator contracts. See
[the importer runtime scan evidence](operations/2026-08-24-importer-runtime-scan.md).
The final image build, contract, scan and three-image manifest still require one
new exact-SHA GitHub Actions run.

Run `32678530047` then tested clean TypeScript-only source `79d2b08`. Every
pre-publication gate passed, including the formal exact-SHA memory acceptance,
and the hardened Alpine importer publish/scan/provenance verification succeeded
at partial digest
`sha256:3584b438e242ebf9d3879c18cea357855e61de64f3d3cc0a8440f3d8d42e1a6b`.
The plugin-installer publish failed before producing an image because its
Debian mirror default used plain HTTP and the runner received HTTP 403 responses
from TUNA; the service publish was cancelled immediately to conserve CI time.
No combined manifest exists and every partial digest remains forbidden for
deployment. Candidate `3144183` removed the vulnerable final Debian utility
layer, but its packaging run `32684971435` exposed a certificate bootstrap
error: Debian slim had no CA bundle before the custom HTTPS apt mirror was
contacted. That run was cancelled after about one minute.

Run `32686030562` then tested clean SHA
`16e46820645c2a8b37ae660fed1fde488ff4b330`. Every source, packaging and formal
memory gate passed, the importer published/scanned successfully, and the plugin
image built. Its unchanged final-image Trivy policy correctly rejected
`libssl3t64` from the distroless `cc` base and fixed HIGH findings in the
official Cosign 3.1.3 binary's Go 1.26.4 standard library, x/mod, x/text and
gRPC. The service build was cancelled immediately. Partial plugin digest
`sha256:c490747f83c926218db357419b193d63b11abcfd43fb3238803374d8c6a41238`
and all other partial outputs remain forbidden.

The current candidate uses the distroless Debian 13 `base-nossl` runtime and
copies only the Rust binaries' required `libgcc_s.so.1`. Because upstream has no
post-3.1.3 release, it reproducibly rebuilds the GitHub-verified v3.1.3 tag
commit as `v3.1.3-mtc.1` using fixed Go 1.26.7 and a checked-in dependency
security patch. A clean-source replay verified the module sums and the resulting
amd64 binary has zero HIGH/CRITICAL findings under the same local Trivy 0.70.0
database. The host has no Docker client, so the real Docker contracts and exact
three final-image Trivy scans remain mandatory in one new exact-SHA CI run.
See [the service and plugin runtime preflight](operations/2026-08-24-release-runtime-preflight.md)
for the exact local linkage and Cosign evidence.

GitHub Actions run `32639275451` tested
`92e23e882771b69be587732eb31eb02ba0a92cc3`. Every job except
`memory-acceptance / optimized-15-minute-harness` passed; GHCR publication and
release verification were correctly skipped. A local exact-SHA full run kept
all memory/resource measurements inside their limits but counted two failed soak
requests. Diagnostic reproduction identified a Node.js 24.18.0 Undici internal
parser assertion under long-lived `fetch` plus `Connection: close`; a subsequent
fully native-HTTP diagnostic also exposed real cross-process SQLite lock waits.
Commit `54fd84f` removed Undici from every harness HTTP phase, waits for
child shutdown, uses WAL snapshots plus immediate SQLite write transactions,
retains a zero-failure gate and records bounded error categories. Its dirty-tree
300-second diagnostic completed 5,538 soak requests with zero failures at
18.449 RPS; every functional and resource check passed, with a 126.359 MiB
gateway lifetime high-water mark against the 224 MiB process budget. This is
root-cause evidence, not exact release evidence. Clean candidate `5cbeaede59b8121165cfcddfecff4ff16774020f`
then passed every functional, 500 MiB asset and 900-second soak check with zero
soak failures, but correctly failed the unchanged 128 MiB stage gates: 12 × 16
MiB streams reached 151.714 MiB and the following two-image phase reached
160.515 MiB. Commit `7210184` bounds simultaneously owned 5 MiB proxy multipart
archive buffers to four without reducing observed upstream concurrency. A
12-stream dirty-tree diagnostic then measured 68.007 MiB for streams, 103.699
MiB for images and 130.945 MiB process high water, all green. Commit `69a23aa`
then passed the final clean 900-second / 500 MiB acceptance profile in run
`32671474947`, as recorded above. No release digests or rollout exist for
`92e23e8` or `5cbeaed`.

The implementation at `92e23e8` already includes the single-pass Responses
visitor, exact HTTP/archive/idempotent-replay image bytes, schema v54 accounting,
all repository scripts in TypeScript, and the converged local Rust/web/operator
gates. The historical status below explains how that state was reached.

GitHub Actions run `32579001708` tested `bd9d2e2`. Every gate except the memory
acceptance job passed. The failing workload was real: two concurrent synchronous
Responses-tool images produced a 152.504 MiB RSS delta against the 128 MiB limit.
Commit `62cab85` fixed that measured path in the short harness. Commit `9eaf4af`
then applied the analogous bounded design to standard OpenAI Images, but no full
CI run has tested either integration commit.

Do not infer release readiness from the targeted results. No new CI, image,
deployment, migration or browser dogfood was started during convergence.

## Historical blocking work completed at `92e23e8`

At the earlier convergence point, the Responses image parser was not ready for
release. It recursively
parses nested `RawValue` subtrees; at the maximum JSON depth a large leaf can be
rescanned repeatedly, turning a bounded 16 MiB body into approximately 2 GiB of
parser work. Replace it with a single-pass root `DeserializeSeed`/`Visitor`:

- process only the normative root `output` recursively and the typed root
  `usage`;
- skip unrelated root fields with `IgnoredAny`;
- keep malformed JSON mapped to `upstream_image_invalid_json` and semantic
  shape errors mapped to `upstream_image_invalid_payload`;
- preserve borrowed byte ranges, duplicate-image rejection, usage allowlisting,
  depth limits and the 16 MiB response bound.

Add a real exact-byte acceptance scenario for both synchronous image protocols.
The first HTTP response body, persisted archive object and same-idempotency replay
must be byte-for-byte equal, and `Content-Length` must equal the shared segment
length. Do not satisfy this only by concatenating segments twice in a unit test.

The standard OpenAI Images path also still needs the short and full concurrent
memory harness. Keep the 128 MiB limit; do not raise it to make the gate pass.

All items in this historical section are now implemented. The 128 MiB image gate
was retained and the final clean acceptance run passed at `69a23aa`; the current
release blocker is the importer runtime image attestation described above.

## Current release resume order

### 2026-08-25 continuation

The working tree after release `84f768e` adds schema v55 semantic execution
metadata and a responsive console pass. Clients can declare session names,
W3C trace/span context, agent ancestry, task kinds and bounded non-secret
metadata; existing Codex session/parent/Merkle evidence remains available when
those declarations are absent. The current continuation additionally projects
session/turn/parent/response/branch/compaction/client evidence as a separate
`structure` object. Session detail renders declared versus inferred request
lanes, an elapsed-request flame view, request-count task distribution and
currency-separated per-agent/per-task billed cost. The Codex custom-provider
configuration and the separate future OTLP event boundary are documented in
`docs/semantic-execution-metadata.md`.
Operator and self-service credentials are remembered in browser-local storage
until the user explicitly clears the corresponding role. Single-tenant service
credentials select their only tenant automatically; creation/onboarding forms
are collapsed until requested and the ordinary-user portal is linked from the
operator header. These changes are local and tested directionally; they are not
yet a pushed release, immutable image, migrated live trial or API2/API3 change.

The isolated API2 candidate built from `84f768e` remains the runtime baseline
while current CPA/CPAMP Longhorn snapshots are imported. API3 is explicitly
outside the current window and must remain unchanged until the user separately
opens the production window. Do not reuse the earlier v54 images for the v55 UI
or schema, and do not trigger CI until the complete local Rust/TypeScript,
fresh-PostgreSQL, browser and migration gates are green.

The importer image now bundles the reviewed TypeScript session-archive exporter
as `/usr/local/bin/export-cpa-session-archive-delta` and installs its `flock`
runtime dependency. The sealed JSONL is still consumed by
`/usr/local/bin/import-cpa-session-archive` from the matching service image.
This closes the observed packaging gap without introducing Python or turning
`audit-cpa-migration` into an importer.

The live scratch dry-run later exposed legacy CPAMP rows whose `failed` flag was
true while `fail_status_code` was 2xx/3xx. The importer must treat the flag as
authoritative: preserve only failed 4xx/5xx codes and normalize every other
failed code to `502`/`upstream_error`. A dedicated PostgreSQL acceptance fixture
is added and must prove the request fact and daily aggregate remain failures
after exact replay. Do not use an older importer digest for the real candidate
apply.

GitHub Actions run
[`32924611225`](https://github.com/memeloop-online/memeloop-token-center/actions/runs/32924611225)
tested clean SHA `888f60fc01780dcddb13738434d32c461ddacede` and completed
successfully. All source, packaging, migration, security, 900-second memory and
three-image publication/verification jobs passed. Its immutable digests are
service `sha256:f2a6fd9f0e228e98e586f333e24028f4b963d458d4847c3e16826d7f5eeb9ffc`,
importer `sha256:d04ef67a310ae490cace21e56079148087011035aeaeade04b56ae2c86dbbec2`
and plugin-installer
`sha256:eef9cedb69ee66db1241d402193ba7853daf2b14cf9e8445684995104a0d35a1`.
The memory gate measured 111.098 MiB lifetime HWM, 70.499 MiB standard Images
delta, 69.719 MiB Responses-tool Images delta, 8.226 MiB for the exact 500 MiB
download and 17,783/17,783 successful soak requests. The packages are
anonymously pullable, but these digests are not deployable: the subsequent real
source dry-run found two lossless-compatibility gaps absent from the synthetic
fixtures.

The real source has one API-key-entry-level private SOCKS5 proxy without proxy
authentication. The current local continuation adds a write-only
`api_key_proxy` credential, encrypted proxy URL, independently pinned target and
proxy endpoints, local-DNS-only SOCKS5 (no `socks5h`/HTTP proxy resolution
bypass), global authority for private-proxy creation/change, ordinary
key rotation that preserves the approved route, OpenAPI/catalog support and a
count-only TypeScript importer migration. The real v0.7.21 archive source also
returns RFC3339 numeric offsets with 5–9 fractional digits, while the later
stable contract requires canonical six-digit UTC `Z`. The exporter now compares
legacy timestamps at nanosecond precision and normalizes only the legacy
projection; stable responses remain byte-form strict.

Current local release evidence after these changes: rustfmt, Clippy with
`-D warnings`, and `cargo test --locked --all-targets --all-features` pass,
including 370 library tests and every integration binary; Rust Cucumber reports
69/69 scenarios and 373/373 steps. Root TypeScript and all five Node files report
45/45 tests, and the OpenAPI route contract remains 106 paths/126 operations.
Web typecheck, 24/24 localization/security contracts, production build and the
Chromium suite (20/20 scenarios, 145/145 steps) pass. The browser suite sends a
real Codex-style semantic request through HTTP and reads it back through the
session API/UI; credential reload/manual clear covers both portals.

A dirty-working-tree short optimized-memory run passed the two unchanged
128 MiB image gates at 95.879 MiB for standard Images and 61.719 MiB for Codex
Responses-tool Images. Both proved exact `Content-Length`, byte-identical replay
and no replay upstream call. The 100 MiB archive download added 0.277 MiB at the
gateway, soak failures were zero, and process lifetime HWM was 123.641 MiB
against the 224 MiB process budget. Root/web npm audits report zero
vulnerabilities and tracked Python remains zero. See
[the retained compatibility evidence](operations/2026-08-26-legacy-source-compatibility.md).
The full test run also exposed a real SQLite deferred-transaction lock-upgrade
race in legacy credential attachment. That path now claims the writer slot at
transaction start, has a deterministic competing-writer regression, and the
formerly intermittent end-to-end scenario passed five focused repetitions plus
both complete Cucumber executions.
This is local regression evidence only. One final exact-SHA CI is still required
before any upstream/archive target write, digest rollout or browser/CLI trial.
At that retained v58 checkpoint the local host had neither Docker/Podman nor
PostgreSQL/Helm, so final-image, fresh PostgreSQL v1→v58 and Helm checks were
still gates for the then-planned run rather than local evidence. This historical
statement is not the current v59 readiness claim.

Run `32943606008` subsequently passed every pre-publication job and the full
memory acceptance for `f3c342027e52c21f455b7c17d201bd8d133b858e`, but the
importer publish job discovered the newly disclosed/fixed CVE-2026-14456 in the
Alpine 3.23.5 base's `libcrypto3`/`libssl3` `3.5.7-r0`. Its partial importer
digest is forbidden and the run produced no complete release manifest. The
current continuation explicitly upgrades those two packages to the v3.23
repository's fixed `3.5.8-r0` before installing runtime tools; the release
contract requires the upgrade and no scanner waiver was added. One new
exact-SHA run must scan the resulting image and produce a complete manifest.

1. Validate the pinned Alpine importer, base-nossl service/plugin runtime,
   patched Cosign and TypeScript operator contracts locally. Do not change
   security or memory thresholds.
2. Push that exact master SHA once and inspect the one GitHub Actions run. GHCR
   publication is allowed only when every required job and final-image security
   scan is green.
3. Record and verify the three immutable GHCR digests and the combined release
   manifest; never deploy the SHA tag.
4. Coordinate
   a CPA/API2 trial deployment with a recorded old-CPA rollback point. Keep API3
   unchanged.
5. Reconcile imported CPA/CPAMP accounts, subscriptions, legacy credential
   access, usage totals, request archives and unlinked-session counts. Dogfood
   operator and self-service flows in a real browser in Chinese/English and
   light/dark modes, including text and image billing.
6. Complete live browser and Codex CLI text/image validation, then report the
   exact source/image digests, addresses, reconciliation counts and rollback
   evidence. Only then release the same digests to API3. A final delta, write
   barrier, destructive route change or rollback-point removal still requires
   its own explicit maintenance approval.

## Worktree inventory at convergence start

- `/home/token-center-dev/worktrees/completion-p0-p1` — integration branch,
  `9eaf4af` before the documentation convergence commit.
- `/home/token-center-dev/worktrees/openai-b64-memory` — clean source branch at
  `37a50fe0d9a2709c036f02deea961bf2ebf0a9a3`.
- `/home/token-center-dev/worktrees/responses-parser-linear` — clean branch at
  `62cab854d7636f1b542797bd97169531ceedc9e3`; intentionally contains no parser
  fix yet.
- `/home/token-center-dev/worktrees/handoff-docs` — documentation convergence
  branch based on `62cab85`; its commit is cherry-picked onto the integration
  branch after validation.

Run `git worktree list --porcelain` and a status check in every worktree before
starting. Preserve unrelated user changes and do not delete worktrees as part of
handoff.

## Product-source map

- Product scope and rejected designs: [Product requirements](product-requirements.md)
- Architecture and trust boundaries: [Architecture](architecture.md)
- Public API: [OpenAPI](../openapi/openapi.yaml)
- JSON Schema configuration: [`schemas/`](../schemas/)
- Plugin ABI and packaging: [Plugin documentation](../plugins/README.md)
- MemeLoop Web entitlement contract: [MemeLoop Cloud integration](integrations/memeloop-cloud.md)
- Storage decision: [Archive storage and SlateDB decision](architecture/archive-storage.md)
- Migration/cutover: [CPA cutover runbook](operations/cutover-runbook.md)
- Trial/release gates: [Deployment readiness](deployment-readiness.md) and
  [Acceptance matrix](acceptance-matrix.md)
- Security: [Security audit](security-audit.md)
- Token Center recovery scope: [Backup and restore](operations/backup-and-restore.md)

If implementation or an older document contradicts the product requirements,
treat it as a defect or explicitly revise the requirement through review. Do not
silently resurrect rejected features such as model-prefix routing, a CPA bridge,
credential-to-provider coupling, application download throttling or a Forgejo/
Harbor release path.

## 2026-08-30 observability continuation

The current working tree extends the existing `/livez`, dependency-aware
`/readyz` and control-only `metrics:read` `/metrics` contract. It adds fixed
label active request/stream/upstream gauges, process CPU/RSS, jemalloc
allocated/active/resident/mapped/retained totals, bounded component memory,
generation/background capacity and aggregate plugin-cache state. Optional
runtime, Google pprof CPU and jemalloc heap diagnostics are registered only on a
control/all role when `MTC_RUNTIME_PROFILING_ENABLED=true`; the gateway and
worker do not register them. Captures are authenticated, process-wide
singleflight, duration/output bounded, `no-store`, shell-free and disabled by
default. No deployment port or ingress is added.

Local evidence includes rustfmt, `cargo check --tests`, the five-test metrics
integration binary, five metrics unit tests, `git diff --check` and an optimized
release build. An explicitly enabled local release process produced a nonempty
non-empty CPU profile under real HTTP load and an 8,380-byte jemalloc heap
profile; the temporary artifacts and SQLite state were removed afterward. An
enabled in-Pod capture remains deployment evidence. Do not trigger a separate
CI solely for this work: combine it with the next necessary exact-SHA release
run after the API2 data-correction and replay evidence has closed.

The first live correction committed all 350,631 reviewed events and rebuilt
the derived aggregates, then exposed an importer orchestration defect: apply
mode continued directly into an all-history ordinary import whose wide
canonical sort exhausted the 10 GiB PostgreSQL PVC's temporary space. That
later replay transaction rolled back and PostgreSQL removed its temporary
files; the committed correction checkpoint, audit and provenance remain
intact. `migrate-cpamp.ts` now returns immediately after a successful
correction. A fresh importer digest must prove a zero-change second correction;
only then should a separate ordinary-overlap replay prove zero imported events.
