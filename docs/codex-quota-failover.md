# Native Codex quota rejection and failover

This change is not a quota query/reset operation, credential refresh, production
route change, or evidence that all accounts can serve every model.

Only native Codex HTTP 429 responses receive additional read-only classification.
The existing authorized-candidate selection, public-model/protocol boundaries,
explicit account hint, three-attempt budget and ambiguous-POST restrictions remain.
A complete structured `usage_limit_reached` error is quota exhaustion; matching
human-readable messages is not evidence. `resets_at` is Unix seconds and takes
precedence over `resets_in_seconds`. A valid future `Retry-After` integer or HTTP
date can extend either exhaustion or ordinary throttling cooldown.

Classification reads at most 64 KiB for at most 500 ms, requires unambiguous JSON
for supplier error fields, and preserves every original response chunk/error.
Incomplete/oversized/malformed error bodies do not establish exhaustion.
Known deadlines are bounded to seven days and at least the configured rate-limit
cooldown. Explicit exhaustion without timing uses a fifteen-minute conservative
cooldown, not an invented reset timestamp. Health stores only a normalized reason
and bounded deadline; raw errors and credentials are never stored there.

Existing health rows and generation/probe fences are reused; no migration is
needed. Concurrent shorter deadlines or ordinary failures cannot shorten a live
quota cooldown. A stale probe success cannot erase the failure after its token
has been cleared. After expiry the existing single half-open probe remains.

HTTP 200 SSE quota-looking errors remain **non-replayable**: zero visible output
and missing usage do not prove zero supplier execution. This patch intentionally
does not turn them into HTTP 429. Implementing an early stream retry requires
separate authoritative evidence of a pre-execution refusal, not a heuristic.
The long-running half-open-probe availability problem is also independent.

Authored CI fixtures cover parser bounds, invalid/duplicate timing and JSON,
byte/error preservation, exact quota failover and next-request skipping, unknown
SSE consumption refusing replay, and SQLite/PostgreSQL two-worker deadline,
generation, single-probe and stale-success fencing. These tests must run in the
existing GitHub Actions Rust/PostgreSQL jobs; no local test or supplier call was
performed while authoring this patch.
