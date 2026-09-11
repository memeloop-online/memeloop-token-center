# Execution and experience acceptance ledger — 2026-09-11

This is the authoritative execution ledger for the MTC cutover and experience
work. It is deliberately a record of evidence, owners, and acceptance
conditions; an implementation branch, a green unit test, a ready pod, or a
single successful request does **not** close an item. Entries marked
**reported** are user observations that still need independently reproducible
evidence. Do not replace them with a more convenient explanation.

## Non-negotiable safety and evidence rules

- No production quota reset, quota-reset preparation, credit consumption,
  credential rotation, source-state deletion, account deletion, or destructive
  CPA action is an acceptance test. In particular, do not burn a paid reset
  credit while diagnosing the refresh/reset UI.
- Do not route Codex traffic directly to `chatgpt.com`. Each Codex OAuth
  account must have an explicitly selected, account-specific `socks5h` egress
  proxy. An official base URL is a provider protocol endpoint, not a proxy
  setting; it must not silently mean direct egress.
- Browser evidence is separate from CI and runtime readiness. Cover cold load,
  revisit/deep link, partial failure/retry, scope changes while loading, empty
  states, stale-response cancellation, keyboard navigation, Escape and outside
  dismissal, focus return, light/dark, Chinese/English, and widths
  320/390/768/1024/1440/1920/2560 without horizontal overflow.
- Filters are anchored **non-modal popovers**: no `aria-modal`, backdrop,
  screen-darkening overlay, or focus trap. Sensitive actions need scoped
  confirmation, concurrency protection, and duplicate-submit prevention.
- Never put a credential, service token, OAuth material, reset credit,
  screenshot of a secret, or raw supplier response in this ledger, a trace, or
  a browser export. Repeat copy must return the current-generation original
  credential, not an identifier, hash, replacement, or a cosmetic “copied” UI.
- Production deploys require an exact green GitHub Actions release, immutable
  image digest, scoped GitOps sync without prune, existing rollback image, and
  post-rollout evidence. Do not switch the working Codex path or retire CPA
  merely because one request succeeds.

## Current evidence snapshot (2026-09-11 UTC)

| Evidence | What it proves | What it does **not** prove |
| --- | --- | --- |
| `origin/master` is `396a329` (`fix(proxy): preserve Codex capacity during recovery`, PR #6). Its full release run `34522522263` was green and produced digest `sha256:886049137c931b8f6b319bef3030871c2908138baaabac85a31415dd67067b80`. | The reviewed PR #6 source passed its listed CI gates and an immutable image was published. | The currently served configuration exposes every required proxy field, all accounts are healthy, or failover is working for every model/account. |
| Scoped canonical-gateway rollout of that digest previously reached two ready pods, with `/readyz` reporting database/archive ready. | That targeted rollout completed at that time. | Current public/private ingress availability, the control-plane configuration, worker state, or an end-to-end authenticated model request. |
| On 2026-09-11, unauthenticated `GET https://token.k3s.onetwo.website/readyz` returned HTTP 404 in 0.18 s. | That public route is not a usable readiness evidence endpoint. | Gateway is unhealthy; readiness must be verified from the workload/endpoints or the documented health route. |
| User report after the rollout: MTC repeatedly returned “no healthy upstream”; Codex CLI was temporarily moved back to the legacy CPA address so work could continue. The user also observed the upstream editor showing an official base URL with no editable proxy field, quota refresh failing, and no usable reset path. | A P0 operational regression remains until independently reproduced and corrected; CPA remains a live dependency. | That a supplier has no capacity, that a reset would fix it, or that a UI message establishes the actual routing cause. |
| Read-only incident evidence before this ledger update found `gpt-5.6-luna` initially routed only to a quota-cooled account. Adding the healthy native Codex OAuth account as an explicit route candidate produced natural Luna successes and logged cooldown-skip/failover. Terra later selected the healthy account but failed local `candidate_prepare`, while the exhausted candidate was skipped. | Candidate omission and a Terra preparation-path defect were distinct causes; a 503 must not be collapsed into “supplier unavailable.” | Terra is fixed, the candidate-preparation reason is fully observable in production, or all account/model pairs have correct bounds/proxies. |
| At 06:03 UTC, two native Codex accounts had their local `quota_exhausted` cooldown records cleared. This was a local routing/breaker-state action; it did not call a supplier quota-reset/prepare/credit endpoint. | The next natural request may re-evaluate those candidates rather than waiting for the old cooldown deadline. | Supplier quota, reset credits, valid authentication, proxy health, route eligibility, or successful failover. Natural traffic acceptance is still pending. |
| The `token-operator` trial control rolled from image `d64` to verified PR #6 digest `886049…` / revision `396a329`; its sole replica is Ready (`1/1`). | That the targeted control rollout reached Ready on the verified image. | Authenticated Operator page/API behavior, actual account configuration, quota read, or gateway/worker cutover. User browser acceptance is still pending. |
| Proxy audit: the two native Codex accounts currently select distinct `socks5h` proxies and inference fails closed when proxy selection is absent/invalid. The health, catalog, quota, and reset paths can still directly connect when their proxy is missing. | Inference-side configuration is not the immediate direct-egress cause for those two accounts; the non-inference paths have a concrete P0 bypass gap. | A complete no-direct-egress property for Codex, including OAuth lifecycle, discovery, health, quota and reset. |
| Migration diff: source has 2 Kimi accounts, 1 Copilot pointer and 1 Cursor pointer; target has 0 corresponding native accounts. Three specified Codex authorizations are also absent. Of 350,631 source body records all are gap-marked; 490,264 archive records are not imported; CPAMP delta reconciliation is open; of 11 active keys, only 10 are recoverable. | The migration has quantified shortfalls and CPA cannot be retired. | That pointer records are reusable OAuth secrets, a body gap is acceptable history parity, archive absence is harmless, or the one unrecoverable active key has a resolved disposition. |

## P0 — restore native MTC availability before CPA retirement

| ID | Status / owner | Required work | Current evidence | Acceptance condition |
| --- | --- | --- | --- | --- |
| P0-AVAIL-01 | **Active — reliability owner** | Reproduce and eliminate recurring `503 no healthy upstream` and terminal client `429` for native Codex routes. Preserve request delivery semantics: only definite pre-delivery failures may retry, and ambiguous/visible-output requests are never replayed. | Luna route candidate omission was corrected in the control plane; natural Luna requests then succeeded. Terra still had a local `candidate_prepare` rejection on the healthy account. At 06:03 two local `quota_exhausted` cooldowns were cleared without supplier reset; natural traffic validation remains pending. | Per route/model and enabled Codex account, authorized candidates have an explicit configuration/transport verdict; capacity exhaustion cools one account and advances to another authorized healthy account without leaking a transient 503/429 downstream. Independent authenticated natural traffic plus logs prove the result for Luna, Terra, Sol, and Astra. |
| P0-AVAIL-02 | **Active — Codex transport + control-plane owner** | Make the selected per-account `socks5h` proxy visible, editable, validated, persisted with revision/CAS, and used by **every** Codex outbound path: inference, OAuth lifecycle, health, catalog, quota and reset. Reject direct Codex egress and accidental official-base-URL-as-proxy configuration before a request is sent. | Audit confirms two accounts currently have distinct `socks5h` proxies and inference fails closed, but health/catalog/quota/reset can directly connect if proxy is absent. User reports proxy is not editable in the form. | A browser can select an allowed proxy for each Codex OAuth account, save/reload it without secret exposure, and show the non-secret effective egress summary. Tests and non-consuming transport inspection prove all six path classes use that proxy or fail closed; no direct `chatgpt.com` route is possible. |
| P0-AVAIL-03 | **Active — routing-policy owner** | Expose and enforce dynamic same-group selection, cooldown, retry, timeout, health/revalidation, and plugin policy. An account hint is an authorized ordering preference, never a hard filter or permission expansion. | PR #9 is green/clean but not merged or deployed. The 06:03 local cooldown clear only reopens candidate selection; it is not supplier reset or availability evidence. | Runtime-editable policy has bounded, documented defaults; authorized candidate ordering, cooldown skip, half-open recovery, retries, and logs are tested. Plugin changes are versioned, allowlisted, auditable, safely reloadable, and cannot bypass route/tenant authorization. |
| P0-AVAIL-04 | **Active — observability/incident owner** | Capture a sanitized reason for every candidate rejection and distinguish: missing candidate, policy denial, proxy/config invalid, local preparation error, network pre-delivery error, supplier quota/cooldown, and post-delivery failure. | PR #10 adds sanitized `candidate_prepare` local-error logging; it is green/clean but not merged/deployed. Current user-facing 503 is too coarse to diagnose. | A sampled incident has one stable request/route/account correlation ID and structured reason chain without secrets; dashboards/queries show candidate count, skip reason, final outcome, retry/failover count, and no duplicate delivery. |
| P0-AVAIL-05 | **Active — release owner** | Verify current canonical gateway, control, and worker desired/revision/image/endpoints; repair only with a scoped rollout. Keep rollback digest available and do not full-sync/prune. | Canonical gateway was selectively updated to PR #6 digest. Trial `token-operator` control now runs verified `886049…` / `396a329` and is Ready `1/1`; authenticated Operator acceptance is pending. The former trial worker remained intentionally untouched. | GitOps desired revision, live image digests, ready endpoints, database/archive readiness, and authenticated natural route evidence agree for all three roles. A documented rollback path exists and no user-visible outage window was introduced. |

## P0 — quota, credentials, and safe account operations

| ID | Status / owner | Required work | Current evidence | Acceptance condition |
| --- | --- | --- | --- | --- |
| P0-QUOTA-01 | **Active — provider/quota owner** | Implement/repair explicit read-only quota refresh states: loading, fresh, stale, unsupported, never observed, permission denied, and failed. Preserve source/provider/window/freshness; never label unknown as exhausted or unlimited. | User reports refresh failure prevents reaching a reset control. At 06:03 local `quota_exhausted` cooldown records were cleared; this is neither a quota refresh nor a supplier reset. `docs/upstream-quota-delivery-audit.md` records that the 2026-09-09 baseline lacked normalized quota windows and UI. | Per supported provider/account, the detail/list view renders read data or an exact non-destructive state with retry. A refresh failure leaves existing timestamped data visible and actionable diagnosis; it never mutates quota/cooldown/credential state. |
| P0-QUOTA-02 | **Blocked on implementation, not on user input — quota owner** | Add real upstream quota metadata and capability model (plan/workspace, windows/buckets, remaining/used unit, reset instant, credits, provenance/freshness). Surface all CPA-visible non-secret metadata that has a verified source contract. | Baseline audit says these normalized adapters/DTOs were absent. PR/branch names alone are not proof. | Contract/API/storage/browser tests show multiple windows, missing vs zero, estimate vs exact, stale/error/unsupported, tenant isolation, localization and responsive presentation. A deployed authenticated read-only inspection verifies actual returned fields without a reset. |
| P0-QUOTA-03 | **Blocked on P0-QUOTA-02 — quota owner** | Add a reset button only where server-derived capability proves a real supplier reset/credit operation. Use a two-step explicit confirmation naming account, windows, credit/cost and non-retry semantics; local cooldown clear is labeled separately. | No production reset, prepare, or credit consumption was performed. Existing audit distinguishes CPA local cooldown reset from supplier credit-consuming reset. | Mock-only CI/browser tests verify confirm/cancel/duplicate/uncertain outcome. Production acceptance remains read-only. The control is hidden for unsupported/unknown capability and cannot be made available merely because refresh failed. |
| P0-CRED-01 | **Active — credential/migration owner** | Restore authorized repeated copying of the original client credential for the current generation, with no-store/no-log behavior and an explicit “unrecoverable” state where no envelope exists. | Migration diff narrows the active-key shortfall: 11 active keys exist, only 10 are recoverable; baseline inventory also recorded 31 keys without current recoverable envelope. User reports no copyable credential. | Creation/authorized repeat-copy flows are independently exercised twice in CI and in deployed browser with a synthetic/current test key; the returned secret matches the same generation, not an ID/hash/replacement. The unrecoverable active key has an explicit owner-approved disposition; all historic keys are reconciled or marked unrecoverable with a migration receipt. |

## P1 — upstream editor and complete UI/UX redesign

| ID | Status / owner | Required work | Current evidence | Acceptance condition |
| --- | --- | --- | --- | --- |
| P1-UX-01 | **Active — Astra UI/UX owner + frontend owner** | Redesign every upstream create/edit flow as an intentional information architecture, not individual field patches: identity/type/auth, route eligibility, connection/base endpoint, account-bound proxy, advanced transport/retry, quota capabilities, health/diagnostics, destructive actions. Use progressive disclosure, contextual help, inline validation, save/cancel/dirty/retry states and accessible layout. | User’s direct browser report says the editor is unusable, proxy is absent, and the form is visually/structurally poor. Open PR #5 (“Improve operator forms and routing information architecture”) is unstable: web and rust checks failed. | Full Chromium interaction suite covers create/edit/error/reload/unsaved change/keyboard/mobile/dark/light/Chinese/English. It proves no secret exposure, no accidental direct traffic, no hidden required setting, and no mutation on cancelled dialogs. A design review covers all provider/auth variants, not just Codex. |
| P1-UX-02 | **Active — frontend owner** | Replace every flat model picker, including filter-assistant settings, with one shared searchable keyboard-accessible hierarchical combobox grouped by authoritative provider then account; show catalog freshness/capability evidence without treating arbitrary ID prefixes as providers. | Baseline Settings control was native flat `<select>`. PR #8 improves confusing explicit-model confirmation but is unstable because web check failed; it does not close this full requirement. | All picker call sites use the shared component; provider/account grouping is authoritative, search/arrow/Enter/Escape/focus work, empty/stale/partial/error states are coherent, and no duplicate conflicting “custom model” confirmations occur. Browser/e2e evidence covers both themes/locales. |
| P1-UX-03 | **Active — requests UI owner** | Complete Request live/list/detail projection for lifecycle, completion, request/response protocol, route, upstream account, session/conversation, price/billing/tokens, latency/error/archive state; retain cursor/filter semantics and redact secrets. Replace the current dark modal filter with anchored popover. | Baseline found persisted fields missing/discarded and `aria-modal` dark-overlay filter. User reconfirmed missing fields and incorrect modal/dark experience. | DTO, API, SSE projection and UI show the same allowed request facts; live-to-persisted reconciliation has no silent loss. Filter is a non-modal popover at every width/theme with keyboard/outside/Escape/focus behavior, and all typed predicates/cursor reset work. |
| P1-UX-04 | **Active — overview/catalog owner** | Make Overview’s “main upstream models” a stable current provider/account/model catalog projection, deduplicated by stable identity and labeled human-readably. Include native Kimi/Copilot/Cursor where migrated/active; never conflate duplicate display names. | User reports two “广电国产自部署模型” rows and missing Kimi/Copilot/Cursor. Baseline says overview was a windowed top-ten traffic list rather than a provider catalog. | With fixture and deployed authorized evidence, exact identities eliminate duplicates; zero-traffic active upstreams are intentionally represented or the label clearly says traffic-only. Kimi/Copilot/Cursor status is accurate (active, not migrated, unsupported, or disabled), never silently absent. |
| P1-UX-05 | **Active — performance owner** | Make provider, quota, pricing/metering, plugin, sessions and overview pages independently load their first useful content. Remove duplicate reads, unnecessary schema/usage coupling, N+1/per-row price lookup and unbounded client transforms. | Baseline: Pricing price calls 145/115 ms yet still loading after 20 s; Sessions loading after 20 s; Plugins made two directory GETs. User reports model pricing extremely slow. | Instrumented browser tests and representative production-shaped query plans record request count, timing and partial failure behavior. FUC and complete-content budgets are documented and met without hiding failed/stale data. |

## P1 — migration, archive worker, and CPA retirement

| ID | Status / owner | Required work | Current evidence | Acceptance condition |
| --- | --- | --- | --- | --- |
| P1-MIG-01 | **Active — migration inventory owner** | Produce a sealed, non-secret source-to-MTC inventory/receipt for every CPA account, OAuth state, client/service credential generation, historical request/conversation/archive record, route/group, quota metadata and configuration. Retain source only until per-item reconciliation passes. | Diff: 350,631 source body records are all gap-marked; 490,264 archive records are absent from target; CPAMP delta is unclosed; 11 active keys exist but only 10 recover. CPA is currently still an emergency Codex CLI path. | Counts, stable IDs, hashes/receipts and authorized redacted samples reconcile. Every body/archive gap and the one active unrecoverable key has an owner-approved disposition; CPAMP delta is closed. No bridge/`cpa-` identity is used as a target account name. |
| P1-MIG-02 | **Active — native provider owners** | Complete independent native OAuth/runtime support and account-specific model/quota status for Codex, Kimi, Copilot and Cursor. Do not use CPA bridge/sidecar or source naming as a substitute. | Diff: source has 2 Kimi accounts, 1 Copilot pointer, 1 Cursor pointer; target has zero native counterparts, and three specified Codex authorizations are missing. Pointer records are not OAuth credentials. | Each enabled native account has target-owned credential/state, provider-specific egress/isolation, model catalog and route evidence; all specified Codex authorizations are reconciled. No request relies on old CPA HTTP endpoints, sidecars or bridge names. Unsupported providers show explicit status. |
| P1-MIG-03 | **Active — archive/worker owner** | Verify worker deployment and durable archive spool end-to-end: seal, lease, upload, terminal request completion, backlog/retry/GC observability, and recovery after worker interruption. | `docs/response-archive-outbox.md` states worker/spool gates were not locally run and historical success showed incomplete archive content. Diff finds 490,264 source archive records not imported; trial worker was deliberately left on an older digest. | Exact worker image/schema/config is live; controlled fixture + natural non-destructive traffic demonstrate terminal archive recovery and bounded retry without duplicate billing or replay. Existing archive migration has a verified import/reconciliation or explicit retention decision for all 490,264 records. Queue/backlog/gap states are visible, alertable and recoverable. |
| P1-MIG-04 | **Not started — retirement/release owner** | Decommission old CPA runtime, bridge accounts, `cpa-` direct accounts, legacy routes/config/resources, and `memeloop-token-center-api2-trial` resources only after P0/P1 receipts. | User reports legacy/bridge remnants repeatedly remained and CPA is still the emergency production path. No deletion receipt is present in this ledger. | All migration receipts and native traffic acceptance pass; GitOps/runtime searches have zero prohibited bridge/legacy references; a reversible cutover window and backup/rollback record are signed off. Then perform scoped deletion and record exact resources removed/recoverability. |

## P1 — active PR/release queue

| PR | Current authoritative GitHub state | Required disposition before merge/deploy |
| --- | --- | --- |
| [#7](https://github.com/memeloop-online/memeloop-token-center/pull/7) CI critical-path optimization | Open, clean, all listed checks green at head `eaa69449`. Automated review requested changes, including fragile CLI validation and a possible user-supplied Docker image-tag deletion path. | Security/release reviewer must resolve every review finding, rerun exact checks, prove expensive gates remain mandatory for release, and compare baseline vs optimized wall time/caching. Not production incident remediation. |
| [#8](https://github.com/memeloop-online/memeloop-token-center/pull/8) route-model catalog wording | Open, unstable. All but `web` passed; run `34529690626` web job failed. Automated reviewer recommendation is not a green release gate. | Diagnose/fix web failure, rerun full head CI, then verify it is integrated into the shared hierarchical-picker program rather than a one-screen wording patch. |
| [#9](https://github.com/memeloop-online/memeloop-token-center/pull/9) account-hint failover | Open, clean, all listed checks green at head `e8c2645`. | Independent reliability review plus exact release rollout; verify production plugins/policies cannot remove fallback candidates. Merge/deploy only alongside live reason-chain observability and no-downtime validation. |
| [#10](https://github.com/memeloop-online/memeloop-token-center/pull/10) explicit custom bounds/candidate preparation logs | Open, clean, all listed checks green at head `eb28d0c`. | Independent review, then deploy scoped fix and observe Terra healthy-candidate preparation with sanitized reason chain. Green CI alone does not close Terra or general availability. |
| [#5](https://github.com/memeloop-online/memeloop-token-center/pull/5) forms/routing IA | Open, unstable: web and rust failed in run `34489912978`. | Treat as an incomplete redesign. Repair CI failures and extend scope to all enumerated upstream forms/UX requirements before acceptance. |

## P2 — remaining route-by-route product acceptance

No route below is accepted until it has current exact-release browser evidence;
“Pending” is intentional and must not be edited to complete on the basis of a
fixture or code review alone.

| Surface | Required interactions | Status / owner |
| --- | --- | --- |
| Operator Overview | Range, trends, exact drilldown, provider/account/model identity, empty/stale windows | Pending — overview owner; includes P1-UX-04 |
| Operator Requests | Live/list/details, cursor, typed filters/popover, lifecycle/route/account/session/billing/archive fields | Pending — requests owner; includes P1-UX-03 |
| Operator Sessions | Search, pagination, selection, replay/tool pairing, scope change | Pending — sessions/performance owner |
| Operator Usage | Dimensions, currency, trends, filters, independent metadata loading | Pending — metering/performance owner |
| Operator Generation jobs | Search, details, assets, cancellation confirmation without submitting | Pending — generation/worker owner |
| Operator Providers | Basic list before stats, catalog/account identity, quota/reset windows, health vs traffic | Pending — upstream/quota owner; includes P0-QUOTA-01..03 |
| Operator Routes | Hierarchical model search, groups, coverage/editor, safe confirmation | Pending — routing/UI owner; includes P1-UX-02 |
| Operator Pricing | First useful content, currency revisit, catalog search, independent usage, manual editor | Pending — metering/performance owner |
| Operator Tenants | Navigation, scope selection, editor/dependency-protected actions | Pending — tenancy owner |
| Operator Client credentials | Search/cursor, status, repeated original-key copy, policy/routing editors | Pending — credential owner; includes P0-CRED-01 |
| Operator Service credentials | Scope/status, guarded editor/actions, no production rotation | Pending — credential owner |
| Operator Plugins | One directory read, lazy configuration, one-plugin retry/save scope | Pending — plugin/performance owner |
| Operator Settings | Credential visibility, grouped filter-assistant picker, persistence | Pending — settings/UI owner; includes P1-UX-02 |
| Portal Overview/Requests/Sessions/Usage/Generation/Generate | Auth, scope isolation, responsive/error/revisit behavior, group picker and capability-specific inputs | Pending — portal owner |

## Closure order and update discipline

1. Close P0-AVAIL-01 through P0-AVAIL-05 with current production evidence;
   do not treat reverting the CLI to CPA as a fix.
2. Close read-only quota/credential safety defects and the upstream proxy/editor
   redesign; a reset action remains untested in production.
3. Merge/release PRs only after their stated independent gates; update this
   table with exact final commit, CI run, image digest, deployment evidence and
   residual risks.
4. Reconcile all migration inventories, native provider routes, histories and
   worker/archive evidence; only then retire CPA/bridge/trial resources.
5. Perform full exact-release UI acceptance and retain measurements, failures
   and owner decisions here. Add newly discovered user-visible defects as a
   P0/P1/P2 row immediately; never delete a row merely because a branch exists.

## Direct status references

- [Upstream quota delivery audit](upstream-quota-delivery-audit.md) records the
  pre-implementation field/capability gap and the no-reset boundary.
- [Durable streaming archive outbox](response-archive-outbox.md) records the
  worker/spool safety model and remaining release gates.
- [Native official-runtime design](native-official-runtime-design.md) and
  [native Cursor source migration](native-cursor-source-migration.md) record
  why a source pointer/bridge is not a migrated native OAuth account.
- [Acceptance matrix](acceptance-matrix.md) remains the product-wide evidence
  standard. This ledger adds scheduling/status and never weakens that standard.
