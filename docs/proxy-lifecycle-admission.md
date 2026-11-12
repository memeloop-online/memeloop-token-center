Proxy lifecycle admission uses the shared weighted byte budget. A long stream
does not consume a fixed process-wide request slot, so a sixty-fifth request is
admitted when its memory reservations and credential policy allow it.

The obsolete `MTC_PROXY_LIFECYCLE_CONCURRENCY` environment variable is no longer
read or emitted by the chart. Remove `config.proxyLifecycleConcurrency` from
deployment values before upgrading the chart: its strict schema rejects the
removed setting. Lifecycle admission already uses bytes, so removing this
unused setting does not change request admission.

The active lifecycle metric counts actual owners, including streaming handoff
and cancellation settlement. It returns to zero only after those owners release
their activity guards.

Request byte, JSON, SSE, retained-response and disk budgets remain enforced.
Credential concurrency, authorization, usage reservation and settlement remain
unchanged. This change introduces no upload deadline.

Regression coverage holds 65 small silent streams simultaneously, checks the
exported active metric, cancels one and completes the others, and verifies one
ledger entry and one terminal event per request. A separate regression exhausts
the real byte budget and verifies that requests are still rejected. Existing
pre-header cancellation and streaming handoff tests retain their settlement and
owner-release assertions. Validation runs in GitHub Actions only.
