# Experience acceptance — 2026-09-09

Baseline: `2b01afe`. This is a coverage ledger, not a claim that the product is
fully accepted. CI fixtures, deployed browser interactions, and authenticated
protocol probes are distinct evidence. An inaccessible screen remains unverified.

## Cross-cutting gates

- Every route: cold load, revisit, refresh/deep link, partial failure, retry,
  tenant/auth change while loading, empty state, and stale-response cancellation.
- Light/dark; 320/390/768/1024/1440/1920/2560 widths; keyboard navigation, Escape,
  outside dismissal, focus return, no horizontal overflow, localized copy.
- Filters: anchored non-modal popover; no screen-darkening overlay or focus trap.
- Model choices: shared searchable, keyboard-accessible provider/account grouping
  based on authoritative metadata; do not infer providers from arbitrary model IDs.
- Sensitive mutations: application confirmation, scope recheck, no duplicate
  submission. Never exercise production rotation, deletion, or grants as a test.
- Quota reset: never invoke a reset endpoint during this task, including probes,
  retries, or browser acceptance. Preserve every paid reset credit. Inspect
  source/contracts and test confirmation with synthetic CI fixtures only.
- Loading: report actual request timing/count and critical dependencies. Do not
  use a spinner or hide failed data to claim a performance improvement.
- Secrets: never include them in screenshots, console, traces, browser storage
  exports, or reports. Credential recovery must return the original credential,
  not an ID, hash, replacement, or merely a working copy button.

## Route coverage

| Surface | Route | Required interactions | Deployed acceptance |
| --- | --- | --- | --- |
| Operator | Overview | Range, trends, exact drilldown, stable account/model identities, empty windows | Pending |
| Operator | Requests | Live/list/details, cursor, filters, lifecycle/route/account/session/billing fields | Pending |
| Operator | Sessions | Search, pagination, selection, replay/tool pairing, scope change | Pending |
| Operator | Usage | Dimensions, currency, trends, filters, independent metadata loading | Pending |
| Operator | Generation jobs | Search, details, assets, cancellation confirmation without submitting | Pending |
| Operator | Providers | Basic list before statistics, catalog/account identity, quota/reset windows, health vs traffic, capability-gated reset confirmation without submitting | Pending |
| Operator | Routes | Hierarchical model search, groups, coverage, editor, safe confirmation | Pending |
| Operator | Pricing | First useful content, currency revisit, catalog search, independent usage, manual editor | Pending |
| Operator | Tenants | Navigation, scope selection, editor and dependency-protected actions | Pending |
| Operator | Client credentials | Search/cursor, status, actual repeated original-key copy, policy/routing editors | Pending |
| Operator | Service credentials | Scope/status, guarded editor/actions, no production rotation | Pending |
| Operator | Plugins | Single directory read, lazy configuration, one-plugin retry/save scope | Pending |
| Operator | Settings | Credentials visibility, grouped filter-assistant model choice, persistence | Pending |
| Portal | Overview | Auth, summaries, route navigation, responsive states | Pending |
| Portal | Requests | Search, list/detail, model/session/billing, scope isolation | Pending |
| Portal | Sessions | Sidebar/detail/replay, empty/error/revisit behavior | Pending |
| Portal | Usage | Time/currency/dimensions, partial failure and responsive charts | Pending |
| Portal | Generation jobs | History/detail/asset and safe cancellation dialog | Pending |
| Portal | Generate | Grouped model selection, capability-specific inputs, result/replay/error states | Pending |

## Confirmed baseline defects

- Shared filter builder uses `aria-modal=true`, fixed dark overlay and hard-coded
  dark colors; model selectors have separate flat implementations.
- Persisted request facts are missing from the SSE DTO and/or discarded by the
  frontend projection, including completion, account/route and billing context.
- Pricing waits for unrelated schema/usage data, repeats usage reads and performs
  per-row linear price lookup. Provider and plugin loading has avoidable coupling.
- All 31 inventoried keys lack a current recoverable envelope; 11 are active.
  Restoring encrypted originals requires exact source-key identity reconciliation.
- Overview is a windowed top-ten account/model traffic list, not a current
  provider catalog. Repeated display names need exact runtime identity evidence.
  The 2026-09-11 replica inventory found only three active target accounts
  (two native Codex OAuth and one HTTP JSON), with no native Kimi, Copilot, or
  Cursor account. This is an unresolved migration gap, not a dashboard omission;
  see `docs/operations/upstream-inventory-gap-20260911.md`.
- Canonical Operator returned 403 from the current source. No allowlist/network
  changes are authorized as a substitute for real acceptance.
- A successful Sol SSE request still reported incomplete archive content.
  Readiness and stream success do not establish full archive recovery.

## Root-agent deployed browser observations

The root agent opened all thirteen Operator routes on deployed `2b01afe` in a
real Chromium browser, using the existing authorized control service via
loopback. Screenshots mask inputs and code/secret-bearing elements. A request
interceptor denies reset/rotation and other mutation requests; no quota reset
was attempted. These first-pass reads do not mark all interactions above done.

- Pricing: prices returned in 145 ms and generation prices in 115 ms, but the
  table still displayed loading after a 20-second observation; usage-summary
  had not completed when the page was closed.
- Sessions: after 20 seconds, only tenant/plugin requests had completed and the
  list remained loading. No concurrent load test was performed.
- Plugins: two directory GETs on one page load, confirming duplicated work.
- Requests: list query returned in 410 ms; persisted identity/completion fields
  were absent from the displayed live table.
- Settings: the filter-assistant model control was a native flat select.
- The canonical Operator domain also successfully loaded Settings and its
  authenticated API data in the real browser, without network changes.
  A subsequent Requests navigation did not mount within 20 seconds; earlier
  probes returned 403. Do not generalize the successful page to stable ingress
  acceptance or use loopback results as canonical-host evidence.

Recorded times are individual baseline observations, not p95 values, service
objectives, or proof of a performance improvement. Re-measure the verified
release using the same scope and interaction sequence.

## Release gates

Only GitHub Actions runs product builds/tests. Deploy only an exact fully verified
release, with existing rollback images, scoped GitOps synchronization and no prune.
Do not switch this working Codex environment or remove its existing access path.
No assertion of perpetual availability or full migration is justified by one
successful request. Retain explicit account/model/transport and archive gaps.
