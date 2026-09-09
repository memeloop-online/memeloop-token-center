# Upstream quota: read-only delivery audit

Audited 2026-09-09 against MTC `2b01afe`. This document records gaps, not implemented capabilities. No reset, credit consumption, credential refresh, health probe, or model invocation was performed.

## Source identity

GitOps `apps/cliproxyapi/deployment.yaml:332` pins **linonetwo/CLIProxyAPI `v7.2.128-onetwo.1`**, digest `9167376d446b86ba989d41010de0f63e8153eca42c3e90d7fb236734557b0551`. The adjacent README has an older tag and is not authoritative.

GitOps `apps/cliproxyapi/usage-services.yaml:95` separately pins **linonetwo/CPA-Manager-Plus `v1.11.12-onetwo.3`**, digest `ede2e383c9be731e69614832b7296f27e41489f63f8465f524b0a5a52ae962be`. This is the primary management UI comparison, not an interchangeable upstream fork.

The backend's management-asset updater defaults to router-for-me/Cli-Proxy-API-Management-Center. Its upstream `main` was observed at `ed5f1c48e11ba7335f1e8f676f228c280196af85`; it is a secondary reference only. Persisted panel configuration and the live downloaded HTML revision were not read, so this audit does not assert that revision is deployed.

Primary, pinned references:

- [Backend account projection](https://github.com/linonetwo/CLIProxyAPI/blob/v7.2.128-onetwo.1/internal/api/handlers/management/auth_files.go)
- [Backend local quota/cooldown reset](https://github.com/linonetwo/CLIProxyAPI/blob/v7.2.128-onetwo.1/internal/api/handlers/management/quota.go)
- [Manager quota types](https://github.com/linonetwo/CPA-Manager-Plus/blob/v1.11.12-onetwo.3/apps/web/src/types/quota.ts)
- [Manager Codex quota and credit consumption](https://github.com/linonetwo/CPA-Manager-Plus/blob/v1.11.12-onetwo.3/apps/web/src/services/api/codexQuota.ts)
- [Manager quota behavior and evidence boundaries](https://github.com/linonetwo/CPA-Manager-Plus/blob/v1.11.12-onetwo.3/apps/docs/en/manual/quota.md)
- [Manager durable quota-cooldown record](https://github.com/linonetwo/CPA-Manager-Plus/blob/v1.11.12-onetwo.3/apps/manager-server/internal/model/quota_cooldown.go)

## Field-to-product mapping

| Source field / behavior | MTC storage and API | MTC UI / remaining gap |
| --- | --- | --- |
| Stable auth/account identity, provider, label | `upstream_accounts`; `UpstreamAccountView.id`, tenant, name, driver, auth kind, connection method | Present. Group overview by stable account ID, never merge equal display names. Do not publish source filenames, auth indexes, credential blobs, or email by default. |
| Enabled/disabled and routing unavailable | Account status; `upstream_account_health` consecutive failures, cooldown, probe lease; versioned monitoring health | Present as distinct account/routing signals. Detailed cooldown deadline/reason is not exposed by the availability DTO. |
| Created/updated, credential expiry, last refresh | Account timestamps and `credential_expires_at`; OAuth lifecycle/leases | Created/updated/expiry exposed. No explicit last successful refresh timestamp in account DTO. Refresh capability is not quota-reset capability. |
| Recent success/failure and latest five results | Request/generation facts + hourly/daily rollups; `/internal/v1/upstream-availability` | Present for an explicit tenant/time window, including zero-traffic accounts. Separate from manual probe. |
| Requests, success rate, latency, costs | `MonitoringMetrics` with currency-separated costs | Availability UI presents request count/rate/average/P95. Monetary usage is not remaining supplier quota. |
| Plan/workspace/organization/subscription tier | No normalized upstream quota/profile store or account DTO fields | Missing typed, redacted metadata and provider-specific provenance. Opaque provider `config` is not a quota API. |
| Codex primary/secondary windows, additional feature windows, review windows; used percentage, window seconds, reset instant/offset, allowed/limit reached | No upstream quota window entity or DTO | Missing all windows, per-window meters and reset countdowns. Preserve feature/window identity and absent values; do not infer weekly/5h periods from labels. |
| Codex credits balance/unlimited, overage, spend controls | No supplier-credit DTO; MTC client credit accounts are a different domain | Missing. Do not display customer key balance as upstream credit. |
| Codex reset credit available/applicable counts; individual credit status, grant/expiry | No reset-credit store, capability field, or action endpoint | Missing. Unknown/zero capability must not produce a reset button. |
| Claude utilization, base/weekly/model-scoped windows, extra usage and profile tier | No normalized provider window adapter | Missing. Deduplicate by stable window/scope and freshness; inactive limits must not masquerade as active limits. |
| Antigravity grouped buckets, remaining fraction, reset, subscription tier | No quota adapter/DTO | Missing. Retain bucket/group identity and distinguish fraction from absolute amount. |
| Kimi usage limit/remaining/reset/window metadata | No native managed-Kimi account support or quota adapter | Missing capability, not a cosmetic UI omission. Source account existence or historical Kimi traffic does not prove a usable target adapter. |
| xAI weekly/monthly free allowance versus official paid API identity | No normalized quota/billing evidence DTO | Missing. Paid API identity success must not be rendered as unlimited quota; throughput headers are not included-plan remaining allowance. |
| Passive response-header/body quota evidence, exact versus estimated recovery | MTC archives contain request facts, but no reviewed normalized upstream quota-evidence projection | Missing allowlisted evidence extraction, timestamp/source/freshness, estimate marker and safe error taxonomy. Never expose response cookies or auth headers. |
| Durable quota cooldown reason/window/recover-at/owner/previous disabled state | MTC breaker table exists, but not the equivalent quota-owned disable/recovery record | Missing. A breaker timeout is not a supplier quota reset; auto-recovery must not override a user's manual disable. |
| Quota loading/error/unsupported/stale/never-observed states | No quota read contract | Missing. All must be explicit; unknown is not zero or unlimited. |
| Quota search/sort, meters, window timeline, refreshed-at | No quota UI | Missing. Use existing resource filters, responsive cards, localized exact numbers, absolute time plus countdown; preserve all applicable windows rather than only a top sample. |
| Real supplier reset versus local scheduling clear | Neither is declared in provider lifecycle capabilities | No supplier reset UI or endpoint exists. Must not reuse `can_refresh`, status toggle, breaker reset, or model test. |

The audited MTC contracts are `src/provider/types.rs:67`, `web/src/types.ts:612`, `web/src/types.ts:706`, `src/db/upstream_account_availability.rs`, and migration `0064_upstream_account_health.sql`. Existing `KeyLimitSnapshot.reset_at` fields belong to customer key budgets/rate limits, not upstream subscriptions.

## Reset boundary

Two different source operations have similar names:

1. The backend fork's `ResetQuota` clears local auth-manager quota/cooldown scheduling state. It does **not** establish that supplier quota or reset credits changed.
2. The manager's Codex `resetCodexQuota` first calls the supplier credit-consumption endpoint with a `redeem_request_id`, then reads quota again. That operation spends a real reset credit.

A future MTC implementation needs a server-derived, versioned capability with explicit effect, supported window/scope, read freshness, applicable credit availability and denial reason. Permission must be checked server-side against the exact tenant/account. A button may appear only for a verified real supplier operation, followed by an accessible confirmation naming the account, affected windows and consumed credit/cost. Disable duplicate submits and use durable idempotency/reconciliation; never retry an uncertain consumption under a fresh request ID. Refresh the read projection after success; an unknown result is not success.

No implementation or live invocation is authorized by this audit. CI acceptance of the eventual action must use mocks only; production acceptance must remain read-only unless separately authorized.

## Evidence still required

- Verify the actually served manager/panel revision without reading secret-bearing configuration.
- Confirm each supported account's read-only upstream quota capability and safe payload independently; do not generalize from one provider.
- Implement storage/API/provider adapters before UI can claim parity.
- Add contract tests for exact/estimated reset times, stale data, zero versus unknown, multiple currencies/windows, tenant isolation and no-reset capability; then GHA browser evidence across seven widths and both themes.
- Verify the deployed authenticated UI independently of fixtures. No local product build/test was run for this audit.
